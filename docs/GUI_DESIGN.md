# The pir GUI: a design for full TUI parity

**Status:** implemented. Most rows of the parity matrix in §4 are enforced by a
test named in the row; the four rows marked *(no test)* (S5, S9, C6, G3) are
implemented but pinned only by their source comment, and are called out as such
rather than claimed. `cargo test --features gui` passes **997 tests, 0 failed**
at the time of writing (900 in the default, non-GUI build; 924 with `--features tui`).

**What this is.** The GUI (`src/gui.rs`) is not "a window with a session
button". It is a port of the TUI (`src/tui.rs`) to GTK, and it already is one:
tabbed sessions, a conversation log, history, background jobs, a question
dialog, ~30 slash commands, log-driven streaming. What it is *not* is a
complete port. This document says exactly what is missing, how each gap is
closed, and which test prevents each one from coming back.

`docs/MISSING_GUI_FEATURES.md` is the earlier gap list and stays the record of
what was fixed in the previous pass. This document is the design for the pass
that closes the rest, and it is written so that a reader can check any claim
by running one test.

---

## 1. The method, and why it is not just a wish list

Three things had to be established before a design could be written honestly,
because each one decides what is *possible*:

**1.1 What the TUI actually does.** Read end to end, not from its docs. The
sub-agent inventory was then checked claim by claim against the source, which
mattered: three of the TUI's own claims about itself are false (§3). Designing
against the TUI's documentation rather than its code would have imported three
features that do not exist.

**1.2 What the toolkit can actually do.** The GUI is built on `rustxWidgets`,
which dlopens GTK at runtime. Its GTK surface is ~223 symbols, and the
inventory below was taken from the vendored tree rather than assumed.

**1.3 Whether tests can see the screen.** They can. `App::init()` succeeds in
this environment (`DISPLAY=:0`), so display-backed tests are real tests here,
not skips. `gui_focus_tests::conversation_textview_must_not_capture_keyboard`
already exercises a live `GtkTextView`. That single fact sets the testing
strategy in §5 and is why several rows below have display-backed tests rather
than only string-matching ones.

---

## 2. The toolkit ceiling

The single most important constraint on the design. Verified in
`vendors/rustxWidgets/`:

| Capability | Available? | Consequence for the GUI |
|---|---|---|
| `ScrolledWindow` + scrollbar policy | **yes** (`core.rs:348`) | conversation is scrollable by wheel and scrollbar drag |
| `gtk_scrolled_window_get_vadjustment` | **yes** (`symbols.rs:431`) | the scroll position can be *read* |
| `gtk_adjustment_set_value` | **NO** — zero hits in the whole tree | the scroll position cannot be *set* |
| mouse-wheel / scroll `EventController` | **NO** — no gesture symbols | no programmatic scroll, and no wheel event to intercept |
| NWG (Windows) text hit-test (`EM_CHARFROMPOS`/`EM_LINEFROMCHAR`/`EM_LINEINDEX`) + raw `WM_LBUTTONUP` subclass + `ShellExecuteW` | **yes** (added for G6) | the Windows pane is a plain EDIT control, so the `wxTextCtrl` + `wxTE_AUTO_URL` model applies: URLs are detected at insert time and clicks open them; EDIT cannot style ranges, so links are not blue/underlined there |
| text-view click coordinates (`window_to_buffer_coords`, `get_iter_at_location`) + button-release event | **yes** (added for G6) | link clicks resolve to buffer positions; hover cursors still **NO** (no cursor-symbol plumbing — links are underlined, clicks work, the pointer just doesn't change) |
| text-buffer tags (`create_tag`/`apply_tag`) | **NO** | no per-range colour; `ConvKind` colour is unreachable on Linux |
| `TextView::set_markup` on GTK | **NO** (`backends_gtk_adapter_impl.rs:646-658`) | the palette is Windows-only today |
| `Button`, `CheckButton`, `DropDown`, `Dialog` | **yes** | graphical affordances are buildable |
| `MenuBar` + GMenu model + actions | **yes**, unusually complete | a real menu bar is buildable |
| tooltips | **NO** — zero hits | no hover help |
| tree / list views | **NO** | pickers must be built from `Button`s or `CheckButton`s |
| file chooser | **yes** (`open_file`/`save_file`) | path entry, no filters |

**The consequence that shapes the whole design.** The TUI scrolls by setting an
integer (`state.scroll`, `tui.rs:659`) and its module doc promises
`PageUp`/`PageDown` (`tui.rs:14`) — which are never bound (§3.2). The GUI
therefore **cannot** offer the same mechanism, and should not pretend to. What
it does instead:

- **Reading scroll position** is possible, so **scroll-follow** is: the GUI
  compares the adjustment value before and after an append, and only forces
  back to the bottom when the user was already at the bottom. This is strictly
  *better* than the TUI, which resets `scroll = 0` on every push (`tui.rs:241`)
  and so yanks the user back down mid-read.
- **Keyboard scrolling** is therefore offered as an explicit, honest mechanism —
  a `GuiScroll` model of its own (F4) rather than a fake `PageUp` — and the
  pane is reachable with wheel and scrollbar, which the TUI cannot do.

This is the one place where the GUI is deliberately *not* a line-for-line
port, and §4 row S1 records why.

---

## 3. Three TUI defects, verified

These are not "missing GUI features" — they are TUI bugs found while
inventorying it. They are listed because a parity matrix that copies the TUI
faithfully would faithfully copy these too, and because fixing two of them is
cheap and the GUI should not inherit them.

**3.1 `/reload` is advertised and does not exist.** `HELP_TUI`
(`tui.rs:1990-1996`) lists `/reload`, and `tui.rs:1156` lists it as
completable, but there is no `"reload"` match arm in `handle_command` — it
falls to `other =>` and answers *"unknown command /reload — try /help"*.
The GUI already implements `/reload` correctly. Fixing the TUI is a 10-line
arm mirroring the streaming REPL's (`main.rs:3844`).

**3.2 `PageUp`/`PageDown` are advertised and never bound.** The module doc
says *"Up/Down/PageUp/PageDown scroll the conversation"* (`tui.rs:14`), but
Up/Down are history recall and there is no `[5~`/`[6~` handler anywhere in the
file (verified: zero matches). The GUI gets real scrollback; the TUI's claim
is corrected rather than copied.

**3.3 Background jobs are never reaped in the TUI.** `reap_bg`
(`tui.rs:564`) and `tasks_running` (`tui.rs:585`) are defined and never
called — `grep -n 'reap_bg' src/tui.rs` returns exactly one line, the
definition. Detached handles accumulate for the process lifetime, and the
`#tasks running: N` footer three comments promise is never rendered. The GUI
already reaps (`gui.rs` `reap_background`) and shows the count, so the GUI is
ahead here. The TUI fix is one call site in the main loop.

---

## 4. The parity matrix

Every row is a feature. "Test" names the test that fails if the row regresses.
Rows are grouped by kind, not by implementation order.

### 4.1 Display and scrollback

| # | Feature (from the TUI or the toolkit) | GUI mechanism | Test |
|---|---|---|---|
| **S1** | Scrollback reachable at all | `ScrolledWindow` wrapping the TextView; **read** the v-adjustment each drain | `scrolled_window_can_report_its_scroll_position` (display), `sync_follow_reports_whether_the_pane_must_be_pinned_to_the_bottom` |
| **S2** | Follow-the-tail without stealing the user's place | `follow: bool` in `GuiState`, set false when the user scrolls away, re-armed on scroll-to-bottom; `sync_textview` holds the buffer while frozen (appends would pin GTK to the bottom) and the live path catches up on re-arm | `follow_is_disarmed_by_scrolling_away_and_rearmed_at_the_bottom`, `frozen_sync_holds_the_pane_and_rearming_catches_up` (display) |
| **S3** | Every push resets scroll to bottom (the TUI's actual behaviour, `tui.rs:241`) | only when `follow` is armed — see S2 for why this differs | `a_fresh_state_follows_the_tail` |
| **S4** | 4000-line scrollback cap | `GuiState::push`, already present | `scrolling_back_keeps_the_cursor_inside_the_buffer` |
| **S5** | 200k-line log-drain cap shifts the write cursor | already fixed, documented at `drain_session_log` | *(no test — pinned by the comment at `drain_session_log`)* |
| **S6** | Keyboard scroll for a GUI (replaces the TUI's phantom PageUp) | `GuiScroll` model + `Page_Up`/`Page_Down`/`Home`/`End` in the pane's key handler and the View menu, both through `scroll_page_up/down/home/end`; the frozen branch of `sync_textview` paints the windowed slice (no `gtk_adjustment_set_value` exists) | `gui_scroll_model_is_bounded_and_clamps`, `scroll_commands_move_the_window_and_arm_follow_at_the_edges`, `keyboard_scroll_paints_the_windowed_slice` (display) |
| **S7** | Markdown rendering of assistant text (`md::render`, `tui.rs:1697`) | `md::render` at drain time, so the pane matches the TUI's text | `assistant_text_is_markdown_rendered_like_the_tui` |
| **S8** | Per-line prefix + colour palette (`ConvKind::into_line`, `tui.rs:165`) | prefix glyph always; colour on Windows only (§2) | `kind_colours_match_the_tui_palette` |
| **S9** | `show_thinking` seeded from the agent's persisted setting (`tui.rs:252`) | `GuiState::show_thinking` read at session start | *(no test — the seeding at `spawn_session` is a source-level claim)* |

### 4.2 Status line (the TUI footer, `tui.rs:662-705`)

| # | Feature | GUI mechanism | Test |
|---|---|---|---|
| **T1** | Spinner + status while running | `running_status`, 10 braille frames, advanced per drain | `running_status_shows_a_spinner_and_queued_count`, `spinner_frame_advances_and_wraps` |
| **T2** | Queued-prompt count | `running_status(s, queued)` | `running_status_shows_a_spinner_and_queued_count` |
| **T3** | Background job count (the TUI's missing `#tasks running: N`) | `tasks_running` + `idle_status` | *(no dedicated test — the `#{n} background` field is built by `idle_status`; see T4)* |
| **T4** | Workspace + model while idle | `idle_status` | `idle_status_carries_workspace_and_model_like_the_tui_footer` |
| **T5** | Token usage after a turn | `drain_once` sets `"{in} in / {out} out tokens"` | `token_usage_is_reported_after_a_turn` |
| **T6** | `pir — running` / `pir — idle` pane title (`tui.rs:653`) | window title tracks the running flag | `window_title_reports_running_or_idle` |
| **T7** | Hint line: `[Tab] levels: …` (`update_tui_hint`, `tui.rs:1140`), and for every other draft the completion candidates: one ⇒ `[Tab] completes to /menu`, several ⇒ the list (a failed Tab adds `[Tab] N matches` / `no match`) | a real hint label under the status bar, refreshed on Tab immediately and on the drain tick otherwise | `hint_offers_thinking_levels_then_completion_help`, `a_draft_shows_what_it_can_be_completed_to`, `the_completion_list_is_exactly_what_tab_matches`, `typing_a_prefix_paints_the_completion_hint` (display) |
| **T8** | `✓ DONE :)` done-placeholder (`tui.rs:681`) | shown in the status bar when idle and awaiting input | `done_placeholder_shows_only_when_idle_and_awaiting_input` |

### 4.3 Commands

`docs/MISSING_GUI_FEATURES.md` already added `/checkpoints`, `/rewind`,
`/quarantine`, `/limits`, `/skills`, `/scratchpad`. The remaining work:

| # | Feature | GUI mechanism | Test |
|---|---|---|---|
| **C1** | Every TUI command reachable in the GUI | GUI `COMMANDS` list ⊇ TUI `complete_idle` list | `gui_completion_is_a_superset_of_the_tuis` (new, §6 C-1) |
| **C2** | `/help` mentions every command it implements | `HELP_GUI` is checked against `handled_commands()` | `help_documents_every_command_the_gui_handles` (new) |
| **C3** | `/reload` real in the GUI (TUI defect §3.1) | already an arm; now also in the TUI | `gui_implements_every_tui_command_the_gui_advertises` |
| **C4** | Completion list never offers an unimplemented command | `handled_commands()` ⊇ `COMMANDS` | `every_completable_command_is_actually_handled` (existing) |
| **C5** | Extension commands reachable | `other =>` arm → `agent.run_registered_command` | covered by C4's arm check |
| **C6** | `/quarantine apply` is confirmable | dialog before a security-relevant action | *(no test — the confirmation path is in the `/quarantine` arm)* |

### 4.4 Keyboard

| # | TUI binding | GUI mechanism | Test |
|---|---|---|---|
| **K1** | Enter submits | `connect_activate` | existing |
| **K2** | `Esc` / `Ctrl-C` cancels the turn | `on_key_raw`, shared with `/cancel` | existing (source-level) |
| **K3** | `Ctrl-D` / `Ctrl-Q` quit | `on_key_raw` | existing |
| **K4** | `↑`/`↓` history recall, draft preserved | `HistoryNav` | existing (6 tests) |
| **K5** | `Tab` completion | `complete_idle` + `complete_model_buffer` | existing (7 tests) |
| **K6** | **`Tab` must not be hijacked when idle** (TUI §3.2 defect: it cycles; GUI must not eat GTK focus moves) | only claim Tab when the buffer is completable | `tab_is_only_claimed_when_the_completion_actually_changes_the_buffer` |

### 4.5 Graphical affordances (what makes it a GUI, not a terminal in a window)

These have no TUI counterpart; they are the reason to have a GUI at all.

| # | Feature | GUI mechanism | Test |
|---|---|---|---|
| **G1** | Model picker | `DropDown` fed from `providers` | `model_picker_lists_exactly_the_configured_models` |
| **G2** | A real menu bar (File / Session / View / Help) | `MenuBar` + `SimpleAction` + GMenu model, returned by `build_menu_bar` and appended to `root` above the tabs; `MENU_VIEW_ACTIONS` (scroll to bottom / page up / down, mark finished) built into the same bar via the scroll helpers | `menu_bar_defines_every_action_the_gui_can_perform`, `menu_bar_is_attached_to_the_window`, `view_actions_are_built_into_the_menu` (integration) |
| **G3** | Send / Cancel buttons | `Button` + `on_click`, routed through the same functions as the keys | *(no test — the buttons are wired to `run_foreground_turn` / the cancel flag)* |
| **G4** | Question dialog is answerable | existing `question_dialog` | existing behaviour + `the_ask_renderer_is_installed_before_the_window_is_built` (source guard) |
| **G5** | `focus-out` must not re-grab focus (TUI has no focus; a GUI must not trap the mouse) | only re-focus on tab change | `focus_out_does_not_regrab_focus` |
| **G6** | Links in the agent log are clickable (open in the browser) | `md::extract_links` inventory at push time (`line_links`, parallel to `conv`); `sync_textview` resolves spans against the exact inserted text (`buf_links`, parallel to the buffer — blocks render multi-line, so buffer lines never map 1:1 to model lines); left-button release → `link_hit_at` coords → span lookup → `open_link_safely` (`xdg-open`, http/https only, no shell, detached). **NWG/Windows** (the `wxTextCtrl` + `wxTE_AUTO_URL` model, since the pane is a plain EDIT control): spans resolve against `TextView::plain_line` (the same stripper `set_markup` inserts), a raw `WM_LBUTTONUP` subclass observes clicks without swallowing them, `EM_CHARFROMPOS` hit-tests, and `open_link_safely` calls `ShellExecuteW("open")` with the same http/https-only policy | `link_spans_resolve_against_inserted_text`, `link_spans_use_successive_search_for_repeated_text`, `link_spans_stay_in_step_with_conv_on_push_clear_trim`, `lookup_link_hits_only_inside_a_span`, `click_handler_checks_selection_before_opening`, `opener_only_allows_http_https`, `link_spans_resolve_against_nwg_plain_line_text`, `windows_sync_builds_link_spans_from_the_inserted_text`, `windows_click_handler_guards_like_the_gtk_one` |
| **G7** | New Project: open a project directory (existing, or a new one) | `project_launch::target` resolves/creates the path (`~` expanded, files refused, a missing path only created after an explicit confirm); `project_launch::launch` starts a **new detached pir process** with that cwd (`setsid`, stdio nulled, reaped off-thread) — a new process rather than a repointed tab because `Agent::new`, `session::session_cwd()`, the `Registry` cwd, the session log and `workspace_label()` all read the *process* cwd; reached from the sidebar **New Project** button, `Session ▸ New project…`, and `/new-project [dir]` (same handler). The picker offers the native folder chooser (`FileChooserNative` `SELECT_FOLDER` / NWG `OpenDirectory`) plus a typed path for new directories | `new_project_is_reachable_from_button_menu_and_command`, `project_launch_detaches_and_never_moves_this_process`, `the_sidebar_carries_a_new_project_button` (display), `project_launch::tests::*` (4) |

---

### 4.6 Compaction (`/compact`)

Compaction is the escape hatch from a full context window, so it must exist on
every front end and it must provably fit the window it is rescuing.

| # | Feature | Mechanism | Test |
|---|---|---|---|
| **C7** | `/compact` reachable from the GUI | `handle_command` arm (`status` / `on` / `off` / no-arg), `COMMANDS`, `HELP_GUI`, and two Turn-menu entries; the summariser runs on a worker (`spawn_compact`) and reports through a transcript notice | `compact_is_reachable_from_the_gui`, `goal_and_continue_are_spawned_not_run_inline` (also pins that the arm never calls `compact()` inline) |
| **C8** | `/compact` reachable from the TUI | `complete_idle` + a `handle_command` arm + `HELP_TUI` (it existed only in the streaming REPL) | `compact_is_completable_and_dispatched` (feature `tui`) |
| **C9** | The summary request fits the window it is rescuing | `Agent::compact_transcript` fits the transcript to *window − system − output − reserve*, keeping the newest messages, abridging tool results to head+tail, eliding the older middle, and stating what it dropped | `compact_transcript_is_fitted_to_the_request_window`, `transcript_fitting_keeps_the_newest_messages_first` |
| **C10** | The output budget never over-commits the window, and never loses its tail | `agent::output_budget_for` is the single rule for `max_tokens`, `trim` and `context_budget`; `Client::complete_with` measures its own prompt against the same window | `output_budget_shrinks_as_the_history_fills_the_window`, `context_budget_matches_what_needs_compaction_uses` |

**What was actually wrong.** The reported symptom was

```
compaction failed: HTTP 400: Model does not support this protocol.
```

on a session at ~911k est. tokens with a 1M window. Two independent defects
produced a *failed* compaction exactly when it was needed:

1. **The summariser sent the whole conversation verbatim.** Tool results go
   into `history` at up to 100k chars each (`read_file`), and the transcript
   repeated every one of them. Measured on the session that failed
   (`.pir/sessions/pir-20261019-133811-sh805959.jsonl`): 6 `read_file` results of
   exactly 100 000 chars each — 600k chars ≈ 150k tokens from six calls alone —
   and the history had reached ~911k est. tokens when `/compact` was run. The
   summariser then serialised that same conversation again, verbatim, with tool
   calls written out as JSON and every result repeated, so the prompt exceeded
   the 1M-token window before the provider could answer it.
2. **The output budget reserved the catalog's `max_tokens` unconditionally.**
   For a model whose `max_tokens` equals its context (the `ollama-cloud`
   fallback catalog lists 1M-context models with a 32k output cap, but a
   catalog entry with a 1M output cap — the shape the live gateway reported
   "requested output 131072" against — reserves the entire window), `trim`'s
   input budget became zero; for every other model the reserve was a constant
   while the history grew, so the window's last few percent were unreachable —
   the exact state a session is in when it needs compaction. Asking for *less* output never makes a request
   bigger, so the reserve now shrinks with the history
   (`output_budget_for`), and the request fits by construction.

The gateway's `400` text is opaque either way ("Model does not support this
protocol", "input plus the requested output exceeds the context window"), which
is why this is pinned by arithmetic tests on the *values the agent computes*,
not by matching an error string: `output_budget_for` is asserted to keep
`input + output + reserve ≤ ctx` for the live numbers (918101 input, 131072
output, 1048576 window), and the transcript fitter is asserted to produce a
prompt whose `estimate_request_tokens` plus the output budget fits.

---

## 5. Test strategy

Three layers, chosen per feature because they answer different questions.

**Layer 1 — pure logic, no display.** Status strings, completion, history
navigation, the scroll model, command lists. These are the majority and they
run anywhere. Worth it because the things most likely to regress — a status
string losing a field, a completion list drifting from the dispatch — are
exactly the things that are cheap to make pure.

**Layer 2 — source-level guards.** For claims about *threading* and
*ordering* that cannot be observed without a running window. The existing
`goal_and_continue_are_spawned_not_run_inline` is the model: a blocking call
cannot be seen from outside, but it is visible in the code, and it is the
thing most likely to be "tidied" back. Used for: `spawn_goal` dispatch,
ask-renderer installed before the first turn, scroll-follow not forcing a
scroll, Tab not unconditionally claimed.

**Layer 3 — display-backed.** Live GTK, skipped gracefully when
`App::init()` fails (so CI without a display still passes). Used where the
property *is* the widget's behaviour: the TextView cannot take focus, a
`ScrolledWindow` reports a scroll position, a `DropDown` lists the right
models, the menu bar has the right actions.

Layer 3 is why this design can claim scroll-follow honestly rather than
hand-waving: the adjustment value is readable, so the test reads it.

---

## 6. What was built

Each item names the row it closes and the test that holds it.

| Row | Change | Test |
|---|---|---|
| **S1** | `scrolled_window_vpos()` reads the adjustment; the scroller is retained on `GuiWidgets` | `scrolled_window_can_report_its_scroll_position` |
| **S2, S3** | `GuiState.follow`; armed on append only when the user was at the bottom; disarmed by a real upward scroll, re-armed at the bottom; the frozen branch holds the buffer, the live branch catches up | `follow_is_disarmed_by_scrolling_away_and_rearmed_at_the_bottom`, `sync_follow_reports_whether_the_pane_must_be_pinned_to_the_bottom`, `frozen_sync_holds_the_pane_and_rearming_catches_up` (display) |
| **S6** | `GuiScroll { offset, page }` with `page_up/page_down/home/end`, clamped; wired to keys + View menu via `scroll_page_up/down/home/end`, painted as a windowed slice | `gui_scroll_model_is_bounded_and_clamps`, `scroll_commands_move_the_window_and_arm_follow_at_the_edges`, `keyboard_scroll_paints_the_windowed_slice` (display) |
| **S7** | `md::render` applied to assistant text in `drain_session_log` | `assistant_text_is_markdown_rendered_like_the_tui` |
| **S9** | `show_thinking` seeded from `agent.show_thinking()` | *(no test)* |
| **T6** | window title set to `pir — running` / `pir — idle` | `window_title_reports_running_or_idle` |
| **T7** | hint label + `hint_text(draft, tab_note)`; `completion_candidates`/`completion_hint` show what Tab would complete, so a prefix is discoverable instead of silent | `hint_offers_thinking_levels_then_completion_help`, `a_draft_shows_what_it_can_be_completed_to` |
| **T8** | done-placeholder in the status bar | `done_placeholder_shows_only_when_idle_and_awaiting_input` |
| **T5** | usage string built by a pure helper | `token_usage_is_reported_after_a_turn` |
| **C1** | `gui_completion_is_a_superset_of_the_tuis` | new |
| **C2** | `help_documents_every_command_the_gui_handles` | new |
| **K6** | Tab claimed only when completion changes the buffer | new |
| **G1** | model `DropDown` | `model_picker_lists_exactly_the_configured_models` |
| **G2** | `MenuBar` from `MENU_ACTIONS` + `MENU_VIEW_ACTIONS`, returned and appended above the tabs | `menu_bar_defines_every_action_the_gui_can_perform`, `menu_bar_is_attached_to_the_window` + `view_actions_are_built_into_the_menu` (integration) |
| **G7** | `/menu` itself: the terminal's menu as a clickable strip above the menu bar (`build_menu_strip`, one item per `MENU_DIALOG_ACTIONS` row in the terminal's order, each running `run_menu_action`), the dialog's rows stacked in a vertical box, `/menu` in Tab completion and in `/help`, and a bare `/model` opening the picker instead of printing the model | `every_menu_action_has_a_topbar_entry` (strip is built from the table and runs the shared dispatcher), `slash_menu_is_offered_by_tab_and_help`, `every_gui_command_is_documented_in_its_own_help`, `menu_dialog_lists_every_terminal_menu_action` |
| **G3** | Cancel/Send buttons sharing the key path | *(no test)* |
| **G5** | focus re-grab removed from `focus-out` | `focus_out_does_not_regrab_focus` |
| **G6** | clickable links: `extract_links` + `pango_visible_text` (md), 6 new loader symbols + `TextView::link_hit_at`/`buffer_has_selection`/`connect_button_release` (toolkit), `line_links`/`buf_links` + `resolve_link_spans` + `connect_link_clicks` + `open_link_safely` (gui); **NWG**: `TextView::plain_line`/`char_index_at`/`connect_left_release`/`open_url` (+ `buffer_has_selection`, `has_focus`, `copy_clipboard`) in the nwg adapter, plus the missing Windows layout shims the shared GUI already called | `link_spans_resolve_against_inserted_text`, `link_spans_use_successive_search_for_repeated_text`, `link_spans_stay_in_step_with_conv_on_push_clear_trim`, `lookup_link_hits_only_inside_a_span`, `click_handler_checks_selection_before_opening`, `opener_only_allows_http_https`, `link_spans_resolve_against_nwg_plain_line_text`, `windows_sync_builds_link_spans_from_the_inserted_text`, `windows_click_handler_guards_like_the_gtk_one` |
| **G7** | `src/project_launch.rs` (shared by GUI and terminal `/new-project`): `target()` + `launch()`; sidebar header row with the **New Project** button; picker dialog (current workspace pre-filled, Browse…, Open, Cancel); `Session ▸ New project…`; `/new-project` in both dispatches; `select_folder` threaded through the loader → adapters → `App` | `new_project_is_reachable_from_button_menu_and_command`, `project_launch_detaches_and_never_moves_this_process`, `the_sidebar_carries_a_new_project_button`, `project_launch::tests::*` |
| **G8** | Sidebar status icon: one glyph per session row, left of the title — `▶` running (blue; a turn, a background job, a live pid, or a worker parked on a dialog it raised), `●` stopped-unseen (amber), `○` stopped-seen (dim). `session_tree::TreeStatus` + `tree_status_of` is the shared model (the text `/sessions` rows end with the same state as `[running]`/`[stopped (unseen)]`/`[stopped (seen)]`); `gui::sidebar_row_status` owns the only edge that creates an amber row (running → inactive) and is **sticky** otherwise, so a finished session never re-lights just because the liveness sidecar says "not running" every tick; `SidebarState::acknowledge` settles it on a row click or a tab switch (which also `mark_seen`s the notification feed). The icon is a `Label` with Pango colour markup, created before its `Button` and revealed with `show_child` (rows appear after `window.present()`), and both are hidden when the tab closes | `sidebar_icon_settles_from_running_to_unseen_then_seen`, `sidebar_acknowledge_is_per_session_and_forces_a_repaint`, `sidebar_icon_markup_carries_the_status_glyph_and_colour`, `tree_status_glyphs_and_colours_are_distinct`, `tree_status_of_prefers_running_then_rest`, `build_tree_stamps_status_from_presence` |

### Deliberately not done

- **Per-range colour on Linux (§2).** Blocked on the toolkit: no text-buffer
  tags, no `TextView::set_markup`. The prefix glyph and the status-bar colour
  are what carry the `ConvKind` distinction. Changing the vendored toolkit is a
  separate piece of work with its own risk, and a half-wired colour path would
  be worse than an honest monochrome one.
- **Programmatic scrolling (§2).** `gtk_adjustment_set_value` does not exist
  in the loader. S1/S2 use *read* access only.
- **Mouse-wheel interception.** No scroll gesture symbols exist, so the GUI
  takes GTK's native wheel behaviour rather than intercepting it.
- **A tab that points at another project.** Every workspace-derived value (the agent's
  registry cwd, the session log, `workspace_label()`, the git root, the security policy)
  reads the *process* cwd, so repointing one tab would repoint them all. G7 therefore
  opens a new process; making cwd per-tab is a much larger change across code shared
  with the TUI and REPL, and would change what the multi-project sidebar means.
- **Link styling on Windows.** The NWG pane is a plain EDIT control: clicks work (G6), but it
  cannot colour or underline a range, so links there look like ordinary text. The
  `wxTE_RICH2` equivalent (switching the pane to NWG's `RichTextBox`, `CFE_LINK` +
  `EM_AUTOURLDETECT`) would give styling and is the natural next step; it is a separate
  change because it reworks the pane control, not just the click path.
- **Link hover cursor.** Clicks work (G6), but the pointer does not change
  over a link — no cursor-symbol plumbing exists in the loader, and that is a
  separate piece of work.
- **Tooltips.** No tooltip symbols exist.
- **The TUI fixes in §3.** Recorded and verified, but §3 is the TUI's bug list,
  not this pass's scope; the GUI already handles all three correctly.

---

## 7. How to verify this document

```bash
cargo test --features gui --bin pir -- --test-threads=8
```

Every §4 row that names a test names one that appears in that run; the four rows
marked *(no test)* (S5, S9, C6, G3) are implemented but not yet pinned by one, and
are labelled as such rather than counted. To re-check that claim after a change:

```bash
# every test named in §4 must exist; the grep prints nothing when the doc is true
for t in $(grep -oE '`[a-z_]{12,}`' docs/GUI_DESIGN.md | tr -d '`' | sort -u); do
    grep -rq "fn $t" src/ || echo "doc names a missing test: $t"
done
```

(`src/` only: identifiers the grep reports — `connect_activate`,
`gtk_adjustment_set_value`, `gtk_scrolled_window_get_vadjustment`,
`window_to_buffer_coords`, `get_iter_at_location`, `buffer_has_selection`,
`connect_button_release` — are toolkit symbols defined under `vendors/`, not
tests, and are expected to be absent from `src/`; likewise the nwg adapter's
`char_index_at` and `connect_left_release`, and `select_folder` (the folder
chooser added for G7).)

Three of the §3 claims are verifiable in one line each:

```bash
grep -n 'reap_bg' src/tui.rs            # 1 line: defined, never called
grep -n '\[5~\|\[6~' src/tui.rs         # 0 lines: PageUp/Down never bound
grep -n '"reload"' src/tui.rs           # no match arm in handle_command
```

If a future change makes one of those greps change shape, the claim in §3 is
stale and the document needs updating — which is the point of writing the
grep rather than the conclusion alone.
