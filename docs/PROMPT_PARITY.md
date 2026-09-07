# Prompt & Output Parity with pi

**Status:** design proposal
**Scope:** what we add to pir's prompts to be more like pi, and how we test that
our *output* matches pi where it should (and diverges where it must).

---

## 1. Goal

pir is a "lightweight Rust reimplementation of pi". Users switching between the
two should get a *consistent feel*: the model should behave the same way given
the same task, and the terminal output should render the same way. But pir is
**not** pi — it has a different tool set, a different security model, and a
different identity. So we want **parity where it helps the model behave well**
and **deliberate divergence where pir is genuinely different**.

This document:
1. Lists what pi adds to its prompts that we should adopt (adapted, not copied).
2. Notes explicitly that we should give help on **PIR**, not pi.
3. Designs a test harness that compares our output to pi's, and states where
   output should match vs. diverge.

---

## 2. What pi adds to its prompts (and what we should adopt)

### 2.1 pi's system prompt structure

pi's system prompt (`packages/coding-agent/src/core/system-prompt.ts`) is:

```
You are an expert coding assistant operating inside pi, a coding agent harness.
You help users by reading files, executing commands, editing code, and writing new files.

Available tools:
- read: Read file contents
- bash: Execute bash commands (ls, grep, find, etc.)
- edit: Make precise file edits with exact text replacement, including multiple disjoint edits in one call
- write: Create or overwrite files
- ls: List directory contents
- grep: Search file contents for patterns (respects .gitignore)
- find: Find files by glob pattern (respects .gitignore)

In addition to the tools above, you may have access to other custom tools depending on the project.

Guidelines:
- Use bash for file operations like ls, rg, find
- Use read to examine files instead of cat or sed.
- Use write only for new files or complete rewrites.
- Use edit for precise changes (edits[].oldText must match exactly)
- When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls
- Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.
- Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.
- You can inspect PI_* environment variables for current model and session details.
- Be concise in your responses
- Show file paths clearly when working with files

Pi documentation (read only when the user asks about pi itself, its SDK, extensions, themes, skills, or TUI):
- Main documentation: ...
- When asked about: extensions, themes, skills, prompt templates, TUI components, keybindings, SDK, custom providers, models, packages, environment variables
- Always read pi .md files completely and follow links to related docs

<project_context>
Project-specific instructions and guidelines:
<project_instructions path="AGENTS.md">...</project_instructions>
</project_context>

[skills section]

Current working directory: /path
```

### 2.2 What pi adds that we should adopt (adapted)

| pi feature | pir today | Adopt? |
|---|---|---|
| **Tool list with one-line snippets** | pir lists tools only via the provider `tools` array (JSON schema), not in the system prompt | **Yes** — add an `Available tools:` block with one-line snippets so the model knows what's available without reading schemas |
| **Tool-specific guidelines** | pir has generic rules only | **Yes** — add per-tool guidelines (e.g. "use read_file instead of cat", "prefer edit_file over write_file", "keep oldText small and unique") |
| **"Be concise" / "Show file paths"** | pir has "Be terse" | **Yes** — keep pir's terse rule, add "show file paths clearly" |
| **Project context (`<project_context>` / AGENTS.md)** | pir appends `# Extra instructions (AGENTS.md)` | **Yes** — keep, but format like pi's `<project_instructions>` block |
| **Current working directory** | pir has `- cwd:` in Environment | **Yes** — keep, it's already there |
| **Skills section** | none | **Maybe later** — pir has no skill system yet; skip for now |
| **pi-docs pointers** | none | **No** — see §2.3 |
| **`PI_*` env var hint** | none | **No** — see §2.3 |

### 2.3 What we should NOT copy verbatim

**Give help on PIR, not pi.** pi's system prompt is heavily self-referential: it
tells the model to read pi's own docs, resolve `docs/...` under pi's install,
and inspect `PI_*` environment variables. If we copy that verbatim, the model
will (a) try to read pi's docs that don't exist in pir's tree, (b) reference
`PI_*` vars pir doesn't set, and (c) answer "how do I use pi?" questions with
pi-specific answers that are wrong for pir.

Instead, pir's system prompt should:

- Keep its identity line: **"You are pir, a minimal terminal coding agent (a
  lightweight Rust reimplementation of pi)."**
- Add a **PIR documentation** section (mirroring pi's structure but pointing at
  pir's own docs): "When asked about pir itself, its extensions, themes, skills,
  or TUI, read `docs/...` under the pir source tree." This is the *adapted*
  version of pi's docs section — same shape, different target.
- Add a **PIR_* env var** hint only if pir actually sets such vars (it sets
  `PIR_AGENT_AS_INVOKER`, `PIR_HEADLESS`, etc.). If we add the hint, name the
  real vars.
- **Never** say "operating inside pi" or reference pi's package paths.

### 2.4 Proposed pir system prompt (adapted)

```
You are pir, a minimal terminal coding agent (a lightweight Rust reimplementation of pi).

Environment:
- cwd: /path
- platform: linux
- date: ...

Available tools:
- bash: Run a shell command in the project directory
- read_file: Read a UTF-8 text file (truncated to 100k chars)
- write_file: Create or overwrite a file
- edit_file: Replace exactly one occurrence of old_string with new_string
- list_dir: List the entries of a directory (non-recursive)
- job_status: Check on a long-running command that was detached
- job_kill: Stop a detached long-running command
- update_goal: Persist and update the current goal/continuation plan

In addition to the tools above, you may have access to other custom tools depending on the project.

Guidelines:
- Use bash for file operations like ls, rg, find
- Use read_file to examine files instead of cat or sed
- Use write_file only for new files or complete rewrites
- Use edit_file for precise changes (old_string must match exactly)
- Keep old_string as small as possible while still being unique in the file
- Be terse: code, commands, short answers, no preamble
- Show file paths clearly when working with files
- When finished, summarize what changed in a sentence or two

PIR documentation (read only when the user asks about pir itself, its extensions, themes, skills, or TUI):
- Main documentation: <pir docs path>
- When asked about: extensions, themes, skills, prompt templates, TUI components, keybindings, SDK, custom providers, models, packages, environment variables
- Always read pir .md files completely and follow links to related docs

<project_context>
Project-specific instructions and guidelines:
<project_instructions path="AGENTS.md">...</project_instructions>
</project_context>

Current working directory: /path
```

### 2.5 What pi adds to the *user* message (and what we should adopt)

pi's `prompt()` (`agent-session.ts`) does three things to the user's text before
sending it:

1. **Extension `input` handlers** can transform the text.
2. **`_expandSkillCommand`** expands `/skill:name args`.
3. **`expandPromptTemplate`** expands file-based `/template` prompts.

pir currently sends the user text verbatim (`Message::user(text)`). We should:

- **Adopt the concept** of prompt-template expansion (a `/template` that reads a
  file and substitutes args) — this is genuinely useful and pi-compatible.
- **Adopt skill expansion** only if/when pir gets a skill system.
- **Not** copy pi's extension `input` transform hook verbatim; pir's extension
  model is different (see `extensions/pi-extensions`). We can note it as a
  future hook but don't need it for parity.

pi also injects **steering / follow-up messages** while streaming. pir has an
equivalent (`continuations` in `agent.rs`). No change needed.

---

## 3. Testing output parity with pi

### 3.1 The fake-agent harness (restore it)

pir previously had a mock server (`mock_server.py`, port 8765) and a Rust
`MockServer` (`tests/mock_integration.rs`) that recorded every request (path,
auth, full JSON body). Both were deleted from the working tree and from HEAD but
survive in git history (`829a80a`, `809b5b9`). We should **restore and extend**
them into a proper parity harness.

**Design:** a single mock server that:
- Listens on a configurable port (default 8799, matching the existing
  `~/.pi/agent/models-store.json` `local` provider).
- **Records every request** to a log file: the full JSON body (system prompt +
  messages + tools), the auth header, and the request path.
- Returns a **scripted, deterministic** SSE response (configurable per test:
  text chunks, tool calls, thinking blocks, errors).
- Can be pointed at by **both** pi and pir via their respective configs.

### 3.2 Capturing pi's and pir's prompts

The key insight: **the mock server sees exactly what each agent sends.** So the
parity test is:

1. Start the mock server.
2. Configure pi to use it (pi's `~/.pi/agent/models-store.json` or a custom
   provider pointing at `http://127.0.0.1:8799/v1`).
3. Configure pir to use it (already configured via `local` provider).
4. Send the **same prompt** to both.
5. The mock server logs each request body.
6. Diff the two request bodies.

This captures the **system prompt** and the **user message** exactly as each
agent sends them — no need to reverse-engineer the internals.

### 3.3 What should MATCH (assert equality)

These are the parts where pi and pir should behave identically, and where a
diff indicates a real parity bug:

- **The user message text** (after template/skill expansion). Given the same
  input, both should send the same `role:user` content. (pir currently sends it
  verbatim; pi expands templates — so this only matches when no template is
  used.)
- **Tool-call/result message ordering** in the conversation history: both should
  interleave `user` → `assistant(tool_calls)` → `tool` results in the same order.
- **The `Available tools` / `Guidelines` structure**: both should have a tools
  list and a guidelines list in the system prompt. (The *content* differs — see
  §3.4 — but the *shape* should match.)
- **The `<project_context>` / AGENTS.md block**: both should include project
  instructions in the same `<project_instructions>` format.
- **The `Current working directory` line**: both should end the system prompt
  with it.

### 3.4 What should DIVERGE (assert NOT equal, and assert the pir-specific content)

These are the parts where pir is deliberately different, and where a diff is
**expected and correct**:

- **The identity line**: pi says "operating inside pi"; pir says "a lightweight
  Rust reimplementation of pi". Assert pir's line is present and pi's is absent.
- **The docs section**: pi points at pi's docs; pir must point at **PIR's** docs.
  Assert pir's system prompt contains "PIR documentation" (or "pir") and does
  **not** contain pi's package paths (`packages/coding-agent`, `docs/extensions.md`
  under pi's install).
- **The `PI_*` env hint**: pi mentions `PI_*`; pir should mention `PIR_*` (or
  none). Assert pir does not reference `PI_*`.
- **Tool names**: pi uses `read`/`edit`/`write`/`ls`/`grep`/`find`; pir uses
  `read_file`/`edit_file`/`write_file`/`list_dir`/`bash`/`job_status`/`job_kill`/
  `update_goal`. Assert pir's tool list names pir's tools, not pi's.
- **The rules**: pi has "Be concise"; pir has "Be terse" + "summarize what
  changed". Assert pir's specific rules are present.

### 3.5 Output rendering parity

The `docs/TODO.txt` already lists "Do output rendering parity tests." The
existing history has `tests/render_parity.rs`, `tests/terminal_parity.rs`,
`tests/grid_impl_parity.rs` (and `ml_*` variants) that were deleted. We should
restore those and extend them to compare **terminal rendering**:

- Feed the **same scripted SSE stream** (from the mock server) to both pi and
  pir.
- Capture each agent's **raw terminal output** (ANSI escape sequences) under a
  pty.
- Compare the rendered frames.

**Where output should match:**
- Markdown rendering of the same text (headings, bold, lists, code fences).
- Tool-call display (`» toolname` style) and tool-result display.
- The "thinking" spinner / reasoning display.

**Where output should diverge:**
- The prompt/identity banner (pi vs pir).
- The `/commands` help text (pi's vs pir's).
- Any pi-specific TUI chrome (status bar, keybinding hints) that pir doesn't
  have.
- Color palette differences (pi and pir may use different theme defaults).

### 3.6 Test matrix

| Test | What it asserts | Match or diverge |
|---|---|---|
| `system_prompt_identity` | pir's system prompt has "pir", not "operating inside pi" | diverge |
| `system_prompt_docs_section` | pir points at PIR docs, not pi's package paths | diverge |
| `system_prompt_tools` | pir lists `read_file`/`edit_file`/..., not `read`/`edit`/... | diverge |
| `system_prompt_guidelines` | pir has "Be terse" + "summarize what changed" | diverge |
| `system_prompt_shape` | both have `Available tools` + `Guidelines` + `<project_context>` + `Current working directory` | match |
| `user_message_verbatim` | same prompt → same `role:user` text (no template) | match |
| `tool_message_ordering` | same tool-call/result interleaving | match |
| `project_context_block` | both include AGENTS.md in `<project_instructions>` | match |
| `render_markdown` | same markdown → same rendered frames | match |
| `render_tool_display` | same tool call/result → same display | match |
| `render_prompt_banner` | pi vs pir banner differ | diverge |
| `render_commands_help` | `/help` output differs | diverge |

### 3.7 Implementation notes

- **Restore** `mock_server.py` and `tests/mock_integration.rs` from git history
  (`829a80a`, `809b5b9`), then extend the Rust `MockServer` to also log the full
  request body to a file (it already records `RecordedReq { path, auth, body }`).
- **Add a `--record` mode** to the mock server that writes each request to a
  timestamped JSONL file, so a human can inspect exactly what pi vs pir sent.
- **Add a `parity` test module** (`tests/parity.rs`) that:
  - Spawns the mock server.
  - Runs pi (via `node .../cli.js -m local/fake "prompt"`) and pir
    (`target/debug/pir -m local/fake "prompt"`) as subprocesses against it.
  - Reads the recorded request bodies and asserts the match/diverge matrix.
- **Gate the pi subprocess test** behind an env var (`PIR_PARITY_PI=1`) or a
  `#[ignore]` attribute, since it requires pi to be installed (it is, at
  `/usr/local/bin/pi`). The pir-only assertions (system prompt content) can run
  without pi.
- **For rendering parity**, reuse the restored `render_parity.rs` /
  `terminal_parity.rs` harnesses, feeding both agents the same scripted SSE
  stream and comparing pty-captured frames.

---

## 4. Summary

- **Adopt** pi's system-prompt *shape*: `Available tools` (one-line snippets),
  per-tool `Guidelines`, `<project_context>`/AGENTS.md, `Current working
  directory`.
- **Adapt, don't copy**: the identity line, the docs section, and the env-var
  hint must all say **PIR**, not pi. Never reference pi's package paths or
  `PI_*` vars.
- **Adopt** prompt-template expansion (and skills later) for the user message.
- **Test** parity with a restored mock server that records both agents' request
  bodies, plus a match/diverge matrix that asserts the shape matches and the
  pir-specific content diverges.
x
