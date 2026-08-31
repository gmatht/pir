# Dialog / Menu Plan (crossterm alternate-screen modals)

pir's streaming REPL runs on the **normal screen** so the whole session (thoughts,
replies, tool calls) stays in the terminal's scrollback. Transient **modals** are
the exception: they should pop up on the **alternate screen** (via crossterm's
`EnterAlternateScreen`/`LeaveAlternateScreen`), hide the agent's streaming output
while they're up, and restore the normal screen (with full scrollback) on close.

This is the same pattern the TUI already uses. The design rule:

- **Normal screen** = the always-on stream (thoughts, replies, scrollback).
- **Alternate screen** = transient dialogs/menus that must not pollute the stream
  or the scrollback.

crossterm is already in the tree (via `streamdown-ansi`), so no new dependency.

---

## 1. Tool-Approval Dialog (highest priority)

**Problem today:** `TtySink::surface` in `security.rs` does a plain `eprintln!` +
`read_answer` on the agent's **worker thread**. During a turn stdin is in raw
non-blocking mode, so `read_answer`'s `read_line` returns `Err` immediately and
falls through to the empty-string default → **Deny**. Mid-turn asks are silently
auto-denied; the user never actually gets to choose.

**Fix:** a crossterm alternate-screen dialog that runs on the worker thread, swaps
to the alternate screen, reads a key, and restores. Because it owns the alternate
screen, the REPL's raw-mode reader can't swallow the answer, and the agent's
streaming thoughts are hidden behind it.

**Dialog contents:**
- Tool name + verb (`bash`, `write_file`, `edit_file`, `read_file`, `list_dir`).
- The command / path (for `bash`, the full command line).
- Parcel id, risk level, blast radius.
- Reason (if any).
- Actions: `[o] allow once  [s] allow session  [n] deny  [i] info` (info shows
  the full blast radius and re-prompts).

**Behavior:** on close, restore the normal screen. The thoughts that streamed
*before* the dialog stay in scrollback; the dialog itself vanishes cleanly.

---

## 2. `/login` Secret Entry Dialog

**Problem today:** `read_secret` toggles `ECHO` off on the normal screen, so the
API key is typed into the visible stream and could appear in scrollback if the
terminal echoes oddly.

**Fix:** a crossterm alternate-screen masked input field. The key is typed on the
alternate screen (shown as `••••`), never touches the normal screen or scrollback,
and is discarded on close.

---

## 3. Session-Resume Picker (`pir -r`)

**Problem today:** `picker.rs` is a hand-rolled two-pane UI on the normal screen
using `libc::poll`, a `SIGWINCH` handler, and manual CSI/VT100 escape parsing.

**Fix:** move it to the alternate screen and use crossterm's event handling
(arrows, resize, page-up/down) instead of the manual `libc` code. On exit it
cleanly restores the normal screen's scrollback.

---

## 4. Main Menu

A top-level menu (opened with a hot-key, e.g. `ctrl-m` or `/menu`) on the
alternate screen. Options:

- **Resume / pick session** — opens the session picker (see #3).
- **Backgrounded sessions** — see #7.
- **Model** — list + select the current model (mirrors `/model`).
- **Thinking** — cycle thinking level / show-thinking toggle (mirrors `/thinking`,
  `/show-thinking`).
- **Security** — see #5.
- **Settings** — see #6.
- **Help** — see #8.
- **About** — see #9.
- **Quit** — exit pir.

Navigation: arrow keys + Enter, or a single hot-key per item. Esc closes the menu
and returns to the stream.

---

## 5. Security Dialog

Shows the current security posture and lets the user toggle what the OS supports.
For each option, indicate **whether it needs root**, **extra dependencies**, and
**whether it's currently active**.

pir's security options (from `security.rs`):

| Option | What it does | Needs root? | Extra deps? | Notes |
|--------|-------------|-------------|-------------|-------|
| **Security level** | `off` / `guard` / `sandbox` / `strict` / `worktree` | — | — | `SecurityLevel` enum |
| **su-based boundary** | agent confined to its `ai_<project>` sandbox identity | yes (root to set up the user) | `sudo` | `set_su_security`; per-session toggle |
| **Overlayfs write-quarantine** | agent writes land in an overlay `upperdir`, reviewed before apply | yes (must mount overlayfs) | overlayfs in kernel | `quarantine`; falls back to in-process guardrail when it can't mount |
| **Project write-quarantine** | overlay the repo root in worktree mode | yes (mount) | overlayfs | `quarantine-project`; on by default in worktrees |
| **Apt mode** | `ask` / `auto-yes` / `auto-no` for `apt-install` | yes (root to install) | — | `AptMode` |
| **Confirm actions** | `--confirm` / `PIR_CONFIRM=1` | no | — | force prompts for shell/write tools |

**OS support:** on **unix**, overlayfs quarantine + su-based boundary are available
(if root + kernel support). On **windows**, these are unavailable — the dialog
should show them as disabled with a "not supported on this OS" note. The dialog
should probe at runtime (e.g. whether overlayfs can mount, whether the current
user is root) and grey out options that can't work, with a reason.

---

## 6. Settings Dialog

Editable settings (persisted to `~/.pi/agent/settings.json` where applicable):

- **Model** (default model for new sessions).
- **Thinking level** / show-thinking.
- **Done-prompt color** (`donePromptColor`).
- **Markdown renderer backend** (`markdownRenderer`: pulldown / comrak).
- **Incremental markdown** on/off + throttle.
- **Full-auto / confirm** mode.
- **Security** (link to #5).

Each row shows the current value and lets the user change it; changes persist.

---

## 7. Backgrounded-Session Selector

A dialog listing **background jobs + sessions with state**, similar to the
`pir -r` menu but for live/backgrounded work. For each entry show:

- Job id / session name.
- **State** (from the verdict classifier): `running`, `complete`, `waiting for
  input`, `needs retry`, `blocked`, `error`, `interrupted`.
- First/last prompt preview.
- Whether it's from this shell.

**Hot-key to pick the next waiting-for-input session:** a single key (e.g. `ctrl-n`
or `n`) jumps straight to the first session whose verdict is `waiting for input`
or `needs retry`, and resumes it — so the user can quickly drive the next thread
that needs them without scanning the whole list. This is the "drive the queue"
flow: backgrounded turns that stalled waiting for input get picked up in one key.

Actions per entry: **resume** (`/fg` or `pir -r`), **view log**, **cancel**,
**mark finished**.

---

## 8. Help Dialog

A scrollable help screen on the alternate screen listing:

- All `/` commands with one-line descriptions (mirrors `/help`).
- Key bindings (Enter, Esc/ctrl-c, ctrl-d, ctrl-m menu, ctrl-n next-waiting).
- A pointer to `docs/` for the full docs.

---

## 9. About Dialog

Shows:

- **Version** — `env!("CARGO_PKG_VERSION")` (currently `0.1.1`).
- **Git hash** — the commit the binary was built from. Not currently embedded;
  add a `build.rs` that runs `git rev-parse --short HEAD` and exposes it via
  `env!("GIT_HASH")` (falling back to `"unknown"` when not in a git checkout).
- **Build profile** — release/debug, opt-level, LTO.
- **License** — GPL-3.0.
- **Dependencies** — the notable ones (pulldown-cmark, streamdown-parser,
  rustyline, ureq, smol, crossterm).

---

## Other menu options worth considering

- **Usage / token budget** — show current session in/out tokens and the budget
  (mirrors `/usage`).
- **Workspace / project** — show current workspace, switch project, run `/project`.
- **Quarantine status** — show staged writes and offer apply/discard (mirrors
  `/quarantine`).
- **Extensions** — list loaded extensions and their tools/commands (mirrors
  `/ext`).
- **Background jobs** — quick list of running jobs with a "jump to" (part of #7).
- **Recent sessions** — a "recently finished" submenu for quick resume.

---

## Implementation notes

- All dialogs share a small helper: `enter_modal()` / `leave_modal()` that wrap
  `EnterAlternateScreen`/`LeaveAlternateScreen` + raw mode, and a key-reader that
  uses crossterm's `event::read` (so arrows/Enter/Esc work uniformly).
- The streaming REPL stays on the normal screen; only these modals use the
  alternate screen.
- The tool-approval dialog (#1) is the highest-value item — it fixes the real
  mid-turn auto-deny bug. The rest are UX polish.
