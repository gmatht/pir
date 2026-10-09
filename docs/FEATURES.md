# pir features: tools, skills, plan mode, sub-agents

This document covers the capabilities added from the Command Code feature plan:
the new native tools, the session scratchpad, `ask_user_question`, skills, plan
mode, sub-agent delegation, richer file reading, and the provider-independent
web tools. Each section states what it does, how to invoke it, the environment
knobs, and how it behaves when nobody is watching.

**Design rule that runs through all of it:** anything the agent can invoke must
have a defined, non-hanging behaviour when there is no operator attached. A
headless run must never block on a question, a modal, or an approval nobody can
give — it must say so and keep going (or stop explicitly).

---

## 1. `glob` and `grep` tools

Native search tools, backed by ripgrep when it is installed, with a built-in
fallback so they work on a host without `rg`.

| tool | what it does |
|------|--------------|
| `glob` | find files by path glob (`**/*.rs`, `src/*.toml`, `*.md`), newest-modified first |
| `grep` | regex content search: `content` (line numbers), `files_with_matches`, or `count` modes |

Both:

- respect `.gitignore` (via `rg`), skip hidden files, and never descend
  `node_modules`, `target`, `vendor`, `.venv`, `dist`, `build`, etc.;
- filter credential paths (`.ssh`, `.aws`, `.gnupg`, `.pem`, `id_rsa`, …) out of
  their results, so a tree-wide search can't become a way around
  `read: guarded-secrets`;
- paginate with `limit`/`offset` (or `head_limit` for `grep`) and say exactly
  what was cut: `… [pir] showing 1 of 2 paths (offset 0, limit 1)`.

Glob semantics: `**` crosses directories (including zero of them, so `**/*.rs`
matches `main.rs`), `*` and `?` never cross `/`, and `[abc]` / `[!abc]` classes
work. A pattern with no `/` is also matched against the file name, so `*.rs`
behaves the way everyone expects.

`grep` takes the regex **directly** — do not escape it for a shell, and do not
wrap it in `/…/`.

Under the security policy both run as `Op::Search`: read-only, allowed wherever
reads are allowed. They are never confused with `Op::Exec`, so a search is never
refused as "running a command".

---

## 2. Session scratchpad

Every session gets a private writable directory outside the repo:

```
$TMPDIR/pir/scratch/<session-key>/      # e.g. /tmp/pir/scratch/pir-20260924-1200-sh1234
```

It is exported to every child process as **`$PIR_SCRATCHPAD`**, so the agent can
`echo hi > $PIR_SCRATCHPAD/out.json` without knowing the path, and a shell the
operator opens (`!`) sees the same variable.

Use it for intermediate artifacts, generated fixtures, extracted logs,
sub-agent scratch work — anything that must not land in the repo (a file in the
project tree shows up in `git status` and in every subsequent `glob`/`grep`).

| knob | meaning |
|------|---------|
| `PIR_SCRATCHPAD_DIR` | root for all scratch dirs (tests/CI use this to stay out of `/tmp`) |
| `PIR_SCRATCHPAD_KEEP=1` | disable the stale-dir sweep entirely |
| `PIR_SCRATCHPAD_TTL_DAYS=N` | how long an abandoned scratch dir survives (default 30) |

Cleanup is lazy: nothing is removed while the session runs. At startup, dirs
whose newest mtime is older than the TTL are removed; the live session's dir is
never swept, and a dir the agent kept writing into stays fresh (the sweep looks
at the max mtime over the tree, not just the top-level entry).

Command: `/scratchpad` (report path + size), `/scratchpad ls`, `/scratchpad clear`.

---

## 3. `ask_user_question`

Lets the model pause a turn and ask a **structured** question instead of
guessing or stopping with free text. The answer is returned as the tool result,
so the model resumes with the decision in context.

```
ask_user_question({
  questions: [{
    header: "Approach",
    question: "Which approach do you prefer?",
    options: [
      { label: "Option A", description: "Description A" },
      { label: "Option B", description: "Description B" }
    ],
    multiSelect: false
  }]
})
```

- Up to 4 questions per call, up to 12 options each. Omit `options` for a
  free-text question. A single-question shorthand (`{question, options: ["a","b"]}`)
  is accepted too.
- Interactive sessions get an alternate-screen picker: arrows + `Enter`, number
  keys `1`-`9`, `Space` to toggle in multi-select, `Esc` to dismiss.
- When a modal can't run (not a tty, or `PIR_NO_MODAL=1`) it falls back to a
  numbered line prompt, so piped/scripted sessions still work.
- **Headless** (`pir "<prompt>"`, `-bg`, sub-agents): there is no operator, so
  the tool returns an explicit "no interactive user" result that tells the model
  to state its assumption and continue (or end with
  `request_stop(reason=stop-needs-input)`). It never blocks.
- `PIR_ASK_TIMEOUT_SECS` bounds the wait for an interactive answer (default
  120s). A timeout is reported as `TimedOut` — **distinct from "dismissed"** —
  and the abandoned dialog is signalled to tear itself down, so a question
  nobody answered never leaves a modal on screen owning the keyboard.

Threading: a turn runs on a worker thread while the REPL owns stdin, so the
question travels back to the REPL over a channel (`src/askuser.rs`) and the
answer travels back the same way. There is exactly one renderer slot, installed
by the REPL at startup.

---

## 4. Skills

A **skill** is a directory with a `SKILL.md` whose frontmatter carries a `name`
and a `description`. The description goes into the system prompt as a one-line
catalog entry; the **body is pulled into context only when the model asks for
it**, via the `load_skill` tool. That is the point: a dozen detailed procedures
cost a dozen lines of prompt, not a dozen documents.

```
.pir/skills/<name>/SKILL.md          # project-local (highest precedence)
<config>/agent/skills/<name>/SKILL.md   # user, e.g. ~/.pi/agent/skills
~/.pi/skills/<name>/SKILL.md         # global
```

A plain `skills/<name>.md` file works too (no directory needed). A project skill
overrides a user skill of the same name.

```markdown
---
name: code-review
description: Perform thorough code reviews
---
# Code Review Checklist
...
```

- `load_skill` with a name returns that skill's body; with no name (or `all`) it
  returns the catalog.
- Names are matched case-insensitively and by unique prefix (`load_skill("code")`
  finds `code-review` when nothing else starts with `code`); an ambiguous or
  unknown name returns an error that lists what *is* available.
- `.pir/skills` is discovered fresh on every call, so a skill written mid-session
  is immediately loadable.
- Frontmatter is parsed by a small dependency-free reader; without frontmatter
  the first heading becomes the name and the first prose line the description.

**Bundled skill: `locate`.** Ship a skill that teaches the search tools and the
order of attack (cheap and broad first, then narrow by structure, then read the
2-3 relevant files): glob/grep recipes for definitions vs. call sites,
config/registration hunts, and the widening order to use when a search returns
nothing. It is written once into `<config>/agent/skills/locate/SKILL.md` on first
run and **never overwritten**, so an edited copy survives upgrades.
`PIR_SKILLS_NOSEED=1` disables seeding.

Command: `/skills` (list, with source and path), `/skills <name>` (print).

---

## 5. Plan mode

A workflow posture: read and search all you like, **change nothing** until the
approach is agreed.

```
/plan                 # status
/plan on | off        # toggle
/plan <task>          # enter plan mode and pre-fill <task> as the next prompt
/plan list            # saved plans
/plan show [name]     # print a saved plan
/plan dir             # where plans live
pir --secure plan ... # start a session in plan mode
```

In plan mode:

- `read_file`, `glob`, `grep`, `list_dir` work normally;
- `write_file`, `edit_file` and `bash` are refused by the security policy
  (`SecurityLevel::Plan`, parcel `plan-mode`), with a message naming both exits.
  The refusal is **never** surfaced as an approvable prompt — plan mode exists so
  a single stray keypress can't turn "show me your plan first" into "go ahead and
  edit the repo";
- the system prompt carries a `PLAN MODE (read-only)` banner explaining why;
- the model records the approach with **`plan_write`**, which saves the markdown
  plan to `<config>/agent/plans/<slug>.md` and asks the operator to approve it.
  Approval leaves plan mode and hands the plan back for implementation;
  rejection keeps plan mode on and passes the feedback back so the model revises.

Without an operator the plan is still written but plan mode stays on (nothing
gets auto-approved), and the tool tells the model to end with
`request_stop(reason=stop-needs-input)`.

Plan mode is persisted per session (`<log>.plan`), so resuming a planning session
comes back read-only and says so at startup.

---

## 6. Sub-agent delegation

The `agent` tool spawns a child with **its own context window** and returns only
its report. The parent's context stays clean: an exploration needing twenty file
reads costs one tool result, not twenty round-trips.

| `subagent_type` | posture | for |
|---|---|---|
| `explore` | read-only (`SecurityLevel::Plan`) | finding things, answering questions |
| `plan` | read-only, uses `plan_write` | proposing an approach |
| `general` | inherits the parent's posture | doing the work |

```
agent({ description: "Explore the auth module", subagent_type: "explore",
        prompt: "Find all auth-related code and report back with paths and line numbers" })
```

Guarantees:

- **The boundary never widens.** `explore`/`plan` run read-only whatever the
  parent's level; `general` inherits it verbatim.
- **No recursion.** `agent`, `ask_user_question` and `request_stop` are removed
  from a child's tool list — there is no channel to a second operator from inside
  a turn, and no unbounded tree of children.
- **No hangs.** The child runs on its own thread with a wall-clock deadline
  (`PIR_SUBAGENT_TIMEOUT_SECS`, default 600s), so a wedged child is abandoned at
  the deadline and reported as a partial result instead of holding the parent
  forever.
- **No terminal noise.** The child is built quiet: its report travels through the
  tool result, its transcript lands in its own session log.
- A silent child still produces a useful report: "finished without a written
  report — it made 7 tool call(s); the transcript is in …".

`run_in_background` is accepted for compatibility and reported honestly as
running in the foreground in this build.

### Concurrent fan-out

Delegation keeps the parent's *context* small; a batch of tasks keeps its
*wall clock* small too. Pass `tasks: [...]` instead of a single `prompt` and
the children run concurrently, returning one merged report:

```
agent({ tasks: [
  { prompt: "Where is auth handled? Report paths + line numbers",
    subagent_type: "explore", description: "auth" },
  { prompt: "Where are migrations run from?",
    subagent_type: "explore", description: "migrations" },
  { prompt: "Where is the retry loop?",
    subagent_type: "explore", description: "retries" }
] })
```

- **One writer.** Children stay quiet and each writes only to its own session
  log; the batch driver owns the single report, so concurrently-finishing
  children cannot interleave on the terminal or race on a shared buffer.
- **Submission order.** The merged report lists tasks in the order you passed
  them, not the order they finished — so it reads the same every run.
- **Per-child deadlines.** One wedged child is marked timed-out for that task
  only; the rest of the batch still returns usable reports.
- **Capped at 8 tasks**, so a fan-out cannot accidentally open fifty concurrent
  provider requests.

### Choosing a sub-agent's model

`explore`/`plan` tasks accept a `model` selector (e.g.
`cerebras/gpt-oss-120b`) so a high-fan-out read-only sweep can run on a
cheaper, faster model:

```
agent({ prompt: "List every call site of `parse_config`",
        subagent_type: "explore", model: "cerebras/gpt-oss-120b" })
```

This is a safety boundary, not just a preference: because `explore`/`plan` run
read-only, a weaker model there cannot change anything, so delegation down is
safe. **`general` ignores `model` and inherits the parent's** — a subtly wrong
`general` child writes code, and that is the one failure mode worth paying for
the strong model to avoid. An unresolvable selector falls back to the parent's
model and says so in the report.

---

## 7. Richer file reading

`read_file` now does more than `read_to_string`:

| input | behaviour |
|---|---|
| UTF-8 text | returned as-is |
| gzip (`.gz`, `1f 8b` magic) | decompressed via `gzip -dc`, returned as text |
| PDF (`%PDF-`) | text extracted via `pdftotext` (poppler-utils) |
| Jupyter notebook (`.ipynb`) | rendered as readable cells: each cell's source in a fenced block, its outputs, and its errors — binary outputs (images, PDFs) are named rather than dumped as base64 |
| Office doc (`.docx`/`.xlsx`/`.pptx`) | text extracted from the ZIP-of-XML container (via `unzip`), paragraphs/rows on their own lines, entities decoded; the other parts are listed so a multi-sheet workbook isn't silently truncated to sheet 1 |
| image (png/jpg/gif/bmp/webp/ico) | **described**: format, intrinsic dimensions (parsed from the header, no decode), size, path, and how to hand it to a vision model or an OCR tool |
| anything else binary | **described** instead of failing: size, detected kind, and a hex/ASCII preview of the first 256 bytes |

### Why images are described, not inlined

Returning a PNG as base64 in a tool result would be actively harmful: a 1 MB
image is ~1.4 MB of base64, a large fraction of a context window spent on data
most configured models cannot interpret. The file stays on disk. A
vision-capable model reads it by path; anything else gets the dimensions and a
pointer at `base64 -w0`, ImageMagick or tesseract. If a model's catalog entry
ever declares vision support, this is the place to branch on it.

The old behaviour — "stream did not contain valid UTF-8" — told the model
nothing. Now a PNG comes back as "binary file — not UTF-8 text: 7614 bytes, looks
like a PNG image" plus a hexdump, which is enough to decide what to do next.
Kind detection covers PNG/JPEG/GIF/BMP/ICO, ZIP (and docx/xlsx/jar), ELF, MZ,
SQLite, gzip/bzip2/zstd, Ogg, RIFF, PostScript.

When `pdftotext` is missing, the result says so and names the package to install.

---

## 8. Provider-independent web tools

`web_fetch` and `web_search` work with **any** provider — they are not tied to
Ollama Cloud (the `ollama-cloud` extension's tools remain available separately).

- `web_fetch(url)` — GET over http/https, returning readable text. HTML is
  reduced to text with `<script>`/`<style>`/`<head>` bodies dropped, block tags
  turned into line breaks, entities decoded, and links preserved as
  `text <https://url>`. `raw: true` returns the body unmodified.
- `web_search(query)` — returns titles, URLs and snippets from the endpoint in
  **`PIR_WEB_SEARCH_URL`** (a template containing `{query}`, e.g. a self-hosted
  SearXNG at `http://searx.local/search?q={query}&format=json`), falling back to
  DuckDuckGo's no-JS HTML page. Both the SearXNG JSON shape and HTML scraping are
  supported; the scraper unwraps search-engine redirect wrappers and drops
  provider chrome.
- When nothing parses, the tool says so and points at
  `PIR_WEB_SEARCH_URL` — a silent empty result is worse than an explanation.

### The SSRF guard

A fetch tool is an SSRF primitive: the model picks the URL. So:

- only `http://` and `https://`;
- cloud metadata hosts (`metadata.google.internal`, `169.254.169.254`) and
  loopback aliases (`localhost`, `0.0.0.0`, `::`) are refused **by name**, before
  DNS;
- every address the host resolves to must be public — loopback, private
  (`10/8`, `172.16/12`, `192.168/16`), link-local (`169.254/16`, `fe80::/10`),
  unique-local (`fc00::/7`), CGNAT (`100.64/10`), unspecified and multicast are
  all refused;
- **`PIR_WEB_ALLOW=host1,host2`** (or `*`) lifts the address check for named
  hosts, for the operator who really does want the agent to read a LAN service.
  The scheme check is never lifted.

Transport is the host's own libcurl, loaded at runtime (`lsb-curl`), so the
binary carries no static TLS dependency.

---

## 9. Taste: recorded preferences

`AGENTS.md` is a document the operator writes once, in prose. **Taste** is the
complement: small atomic preferences recorded *as they come up*, which are
injected into the system prompt so they apply in every later session.

```
<config>/taste/taste.json   +  taste.md     # user scope (~/.pi/taste/)
<cwd>/.pir/taste/taste.json +  taste.md     # project scope
```

The agent records one with the **`remember`** tool when the operator states a
lasting preference, corrects it in a way that generalises, or it learns
something about how this project must be worked (a required flag, a formatter, a
convention). It must *not* record one-off task details or anything AGENTS.md
already covers.

```
remember({ text: "Never commit without asking",
           evidence: "operator instruction", scope: "project", confidence: 0.95 })
```

- **Reinforcement, not duplication.** Re-recording the same preference (however
  it is worded) bumps its confidence and `hits` rather than adding a second
  entry, so a preference the operator keeps restating becomes more prominent.
- **Confidence** (0.0-1.0, default 0.6) orders the injection and marks anything
  below 0.8 as `(tentative)` in the prompt.
- **Two scopes.** `project` (default) is for preferences learned while working
  in a repo; `user` applies everywhere. Project entries are injected after user
  ones and win on a conflict.
- **Bounded injection.** At most 40 entries go into the prompt, ordered by
  confidence; the remainder are counted, not silently dropped.
- **`taste.md` is editable.** Hand-written `- preference` lines are adopted on
  the next load, so the markdown view is a usable interface rather than a
  write-only artifact. Deleting a line from it does *not* delete the entry —
  the JSON store is authoritative for removals, so a stray edit can't silently
  drop a preference. Use `/taste forget`.

Commands:

| command | effect |
|---|---|
| `/taste` | list every recorded preference, by scope |
| `/taste remember <text>` | record one directly (project scope, confidence 0.9) |
| `/taste forget <slug or fragment>` | remove one |
| `/taste find <text>` | search text and evidence |
| `/taste path` | where the stores live |

---

## 10. Session checkpoints and rewind

`/undo` reverts a single `write_file`/`edit_file`, by hashing that file's pre-edit
contents into a sidecar. That handles an accidental edit, but not the case that
actually hurts: a **turn** that went wrong — three edits and a command — and needs
unwinding as a unit.

A **checkpoint** is opened at the start of every turn. It captures each file the
turn modifies, *once per file per turn* (before the turn's first modification of
that file), plus the conversation length at the turn boundary. Files are stored
as full snapshots, and the index is persisted next to the transcript:

```
<session>.checkpoints.json     the index
<session>.ckpt/cp<N>/…         per-checkpoint file snapshots
```

Rewinding restores **two things that must move together**:

* the **working tree** — every file the turn changed goes back to its pre-turn
  contents, and files the turn *created* are removed (a rewind of "I wrote a new
  module and it was wrong" must delete it, not leave an empty file);
* the **conversation** — history is truncated to where the turn began, so the
  model stops reasoning about work that has been undone.

Restoring only the files is what makes an ordinary undo confusing: the tree is
back but the model still believes it made those edits and builds on them.

```
/checkpoints       # list the rewoundable turns (#1, #2, …) with file counts
/rewind <#>        # restore the tree and the conversation to that turn
/rewind            # same as /checkpoints
```

Because the index is on disk, rewind works **across a crash or a `pir -c`
resume** — which is exactly when you most want it. Ids are never reused after a
reload, and rewinding to `#N` discards every checkpoint after it (they describe a
future that no longer exists).

**What a rewind does not do.** Only files the agent modified through its own file
tools are captured. Side effects of `bash` are deliberately out of scope — there
is no way to know what a shell command touched without a filesystem snapshot
facility, and pretending otherwise would make rewind untrustworthy. A turn that
ran a command is marked, and the rewind result says plainly that those effects
are *not* undone.

History is capped at the last 50 turns; older checkpoints and their snapshots are
pruned, so a long session's checkpoint directory stays in the low megabytes.

---

## 11. MCP servers (Model Context Protocol, stdio)

pir can use tools from **MCP servers** — external programs that speak JSON-RPC
2.0 over their own stdin/stdout. They appear to the model exactly like built-in
tools, namespaced `mcp__<server>__<tool>`.

```
pir  ──spawn──►  server (child process)
 ├── stdin  ────►  {"jsonrpc":"2.0","id":1,"method":"tools/list"}\n
 └── stdout ◄────  {"jsonrpc":"2.0","id":1,"result":{"tools":[...]}}\n
```

### Configuration

`~/.pi/agent/mcp.json` (or `<config>/agent/mcp.json`); the `mcpServers`
spelling used by other clients is accepted too, so a config can be copied here
unchanged:

```json
{
  "servers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
      "env": { "FOO": "bar" },
      "cwd": "/tmp"
    }
  }
}
```

At startup each server is spawned, handshaken (`initialize` →
`notifications/initialized`) and asked for its tools (`tools/list`). `/mcp`
lists the configured servers and the tools actually available this session.

### Starter servers — opt-in, never default

`/mcp init [name...]` writes a config for a known server (or all of them), and
refuses to touch an existing `mcp.json` — silently replacing a hand-written
server list would be a hostile thing to do. `/mcp` with no config lists what is
available:

| starter | what it gives you |
|---|---|
| `context7` | up-to-date library documentation (React, Next.js, …). No API key needed — it publishes `resolve-library-id` and `query-docs`. |
| `filesystem` | read/write under one directory tree, for a scoped sandbox |

**Connecting happens in the background.** The extension registers immediately
with no tools, spawns a thread that starts the servers and handshakes, and
adopts the tools when they arrive (`specs()` is rebuilt each turn, so they appear
as soon as they are ready). A slow server therefore costs nothing at startup:
with Context7 configured and a *cold* npx cache, time-to-first-prompt measured
**0.7s**, where connecting eagerly would have been ~24s. A call made in the
narrow window before the connect finishes returns a clear "still starting"
message rather than hanging.

**Nothing is enabled by default, deliberately** — not for latency any more, but
for cost: a server still spawns a child process and needs network access on
*every* launch, including sessions that never call an MCP tool. It is a
one-command opt-in (`/mcp init context7`), discovered via `/mcp`, at zero cost
to anyone who does not want it.

### Failure policy

Starting a server is *untrusted work* — it may not be installed, may crash, may
hang. None of that may stop pir from running:

- a server that fails to start is reported **once, in the startup banner**, then
  ignored for the session; its tools simply do not exist;
- a server that dies later is **not respawned** mid-session (a crash loop would
  burn a turn each time) — calls against it return a clear error;
- every request has a timeout (`PIR_MCP_TIMEOUT_SECS`, default 120s), so a wedged
  server can never hang a turn;
- nothing is written to a pipe without that timeout applying to the wait.

`PIR_MCP=0` disables the feature entirely — no process is spawned at all.

### Details worth knowing

- **Tool names are sanitized** (`mcp__my-server__read_file`): two servers cannot
  collide, and the model can see where a tool came from.
- **Servers run in their own process group** and are killed with it on exit, so
  an `npx` wrapper and the node process under it both go — no orphaned servers.
- **Non-text content is named, not dumped**: an MCP tool returning an image
  yields `[image content: image/png]` rather than a base64 blob, for the same
  reason `read_file` describes images rather than inlining them.
- **Noisy servers are tolerated.** A server that logs to stdout (against the
  spec, but common) does not break the protocol — non-JSON lines are skipped.
- Only **stdio** is implemented. Remote/hosted servers would need HTTP+SSE,
  which is separable and not done.
- **MCP calls are gated as `Exec`.** An MCP tool runs code inside a spawned
  server process, so it can read files, reach the network and write anywhere —
  it is not a read. It is therefore subject to the security policy like `bash`
  is: allowed under the default posture, refused in plan mode. (Without this the
  whole MCP surface bypassed the guardrail, which mattered most in plan mode,
  where a `mcp__x__delete_file` call would otherwise have changed state under a
  posture whose entire purpose is that nothing does.)

---

## 12. Resource limits for agent commands

Every `bash` command the agent runs is placed in its own **cgroup**, with memory
and process ceilings, and killed *before* the host OOMs or a swap collapse makes
the machine unresponsive. Full design and measurements:
[`RESOURCE_LIMITS.md`](RESOURCE_LIMITS.md).

**The limits are advisory by default.** A `cargo build` that legitimately needs
30 GB must be allowed to take 30 GB, and a static number cannot tell that apart
from a leak heading for 30 GB — OOM pressure is *exceptional*, multi-GB builds
are *ordinary*. So a command's cgroup is created and **measured** but no hard
kernel ceiling is written, and commands are stopped only when the host is
genuinely in danger.

| knob | default | effect |
|---|---|---|
| per-command ceiling | `max(60% of RAM, 4 GiB)` | deliberately loose: ~38 GiB on a 64 GiB box, so a 30 GB build fits. Not clamped by current free memory |
| session ceiling | `max(90% of RAM, ceiling + 1 GiB)` | bounds the aggregate of commands + jobs + sub-agents; stays above the per-command ceiling |
| enforcement | **off** unless `MemAvailable < max(10%, 1 GiB)`, swap >50% used, or an explicit override | the ceiling becomes a hard kernel limit only under real pressure |
| `memory.soft_limit_in_bytes` | the ceiling, always | reclaim *preference* under pressure — it can never kill, so heavy work proceeds while the kernel still prefers our cache |
| swap allowance | `0` per command (when armed), `256 MiB` per session | overshoot kills immediately instead of thrashing |
| `pids.max` | 1024–4096 by host size | a fork cascade cannot exhaust the host PID table |
| CPU | **not limited** | a build *should* use every core; capping it silently would make every build 20× slower |

The ceilings are **derived from the host** (and from the container's own limit,
so a container-limited `pir` does not imagine it has the host's RAM), and
`/limits` shows where each number came from *and* which posture is live —
"advisory — commands are measured; the ceiling is not a kill line" versus
"ARMED — the host is under pressure".

Two things make this more than a limit:

* **It is legible.** A killed command comes back with its budget, the basis for
  that number, the peak, whether the host was in danger, and a hint for the
  common causes (`-j$(nproc)`, `xargs -P`, node leaks). An unexplained SIGKILL
  (exit 137) makes a model retry the same command forever. A command that
  *succeeded* near its ceiling reports its peak too, so the operator can raise
  the budget before the next failure rather than after it.
* **It asks whether the task is to blame.** When the host is under pressure the
  guard compares how fast the tool's memory is growing against how fast the host
  is losing available memory. A task whose usage is flat while the host drains is
  **not** killed — it gets a note saying another consumer is growing, and the
  *next* command's budget shrinks instead. A wrong kill destroys a correct
  `cargo build` and reads as flakiness, so the asymmetry favours not killing.
  The one exception is the hard floor (`MemAvailable < 5% of RAM, min 512 MiB`),
  where the box is about to OOM *something* and the command is stopped for host
  protection — explicitly labelled as **not** an accusation.

`/limits` reports the detected hierarchy, the budgets, the enforcement mode and
the live host state. On a host with no usable cgroup memory controller it says
**"detection only"** rather than pretending: commands are then measured and
host-pressure checked, but not kernel-limited.

Knobs: `PIR_RESOURCE_MEMORY_MIB` (sets a ceiling **and** arms it),
`PIR_RESOURCE_ENFORCE=1` (arm on a healthy host), `PIR_RESOURCE_PIDS`,
`PIR_CGROUP=0` (disable).

---

## 15. The `/menu` settings screen is editable

`/menu` is an alternate-screen picker. Its **Settings** and **Security** items
are editors, not reports: `↑`/`↓` move between rows, `←`/`→` (or `h`/`l`) change
the selected value, `Enter` applies, `q`/`Esc` discards. Everything applies to
the running session immediately and is saved where it can carry into the next one.

| row | changes | persisted as |
|---|---|---|
| model | *(not editable here — `/model` and the model picker own this)* | — |
| thinking | the reasoning level, cycled through only the levels the current model actually offers | `<log>.thinking` |
| show thinking | whether streamed reasoning is displayed | `<log>.thinking` |
| incremental md | in-place markdown rendering vs. one dump at the end of the turn | `<log>.incmd` |
| stop-skill | the explicit-`request_stop` skill | session; `/stop-skill default` persists it |
| full-auto | whether unanticipated requests run without asking | `security.ask` in `security.toml` |

**Full-auto is the ask policy, and nothing more.** It is `security.ask =
auto-yes` — the setting that makes `SecurityPolicy::decide` return `Allow`
before any approval is surfaced. It does **not** relax the security level, the
write-quarantine, or the path guards: a request the policy denies is still
denied in full-auto. Turning it *off* restores `ask`, never `auto-no`, so
"stop asking me" can never quietly become "deny everything".

That row used to be the reason this screen was rebuilt: it was *drawn* as a
value, but the flag behind it was constructor-only (`Registry::full_auto` had
no setter), so there was no key — and no code path — that could turn it on.
`Agent::full_auto()` is now **derived from the live ask policy** rather than
stored beside it, so the number on the screen and the behaviour at tool time
cannot disagree.

The `←`/`→` handler is unit-tested per row (`modal::settings_editor_tests`),
including that the thinking row only ever produces levels the model offers, and
that a resumed session's full-auto reflects its actual posture rather than the
launch-time flag.

---

## 14. zellij session recording and rebuild

When `pir` runs inside a [zellij](https://zellij.dev) pane it records which
session and pane it is in, so a whole zellij session can later be rebuilt with
each `pir` pane restored to the *specific* conversation it held.

zellij already persists layout on its own — a live session can be dumped with
`zellij action dump-layout`, and an exited one keeps its layout at
`~/.cache/zellij/contract_version_1/session_info/<name>/session-layout.kdl`
(what "attach to resurrect" reads). What zellij cannot know is which `pir`
transcript belonged to which pane; a resurrected pane restarts `pir -r` and
resolves its session from `parent_shell_pid()`, which is meaningless after a
reboot. This feature fills exactly that gap.

**Recording.** Once per prompt, `pir` writes a sidecar next to its session log:

```
<session>.jsonl.zellij.json
{ "session": "vitreous-hill", "pane_id": "3",
  "cwd": "/home/ai/src/pir", "captured": 1758888000 }
```

The capture reads `ZELLIJ_SESSION_NAME` + `ZELLIJ_PANE_ID` from the environment
and spawns nothing, so it costs a single small file write per turn. Outside
zellij it is a no-op — no file is written at all.

### The pane title, which is what makes plain `zellij attach` work

The sidecar alone only helps `pir -r @<session>`. For zellij's own
`zellij attach` to bring back the *right* conversation, the identity has to be
inside the layout zellij already persists — so once per process `pir` also
claims its pane's title:

```
zellij action rename-pane --pane-id <ZELLIJ_PANE_ID> pir:<session-stem>
```

zellij serializes that as `name="pir:<session-stem>"` on the `pane` node and
re-applies it when the session is resurrected. Verified end-to-end on 0.45.1: a
live session's persisted layout contained

```kdl
pane command="pir" name="pir:pir-20261003-…-sh1" cwd="home/ai_pir/src/pir" focus=true {
    start_suspended true
}
```

**Why a title and not zellij's `post_command_discovery_hook`**, which is a
supported feature aimed at exactly this problem:

- **The hook cannot tell panes apart.** It runs `sh -c` from the zellij
  *server*. The server's environment carries `ZELLIJ_SESSION_NAME` but **no
  `ZELLIJ_PANE_ID`**, and its cwd is the server's (`PWD=/root`), not the pane's.
  Anything per-pane is therefore impossible from the hook.
- **It costs a fork per active pane per second, forever.** The server drives it
  on a 1 s timer and again on every process change in a pane.
- A pane title is **per-pane data zellij already serializes and restores** — no
  polling, no hook.

Two deliberate limits. The title is set **once per process** (guarded by a
latch), so the prompt path still pays for at most one `zellij` spawn per
session. And a title the operator set by hand is **never** overwritten: only a
title that is empty or already `pir:`-prefixed is claimed. A consequence worth
stating: because the latch holds for the process's lifetime, a `/rewind` to a
*different* transcript does not re-title the pane — the sidecar still tracks the
new transcript, so `pir -r @<session>` stays correct, and only the title goes
stale until the next `pir` starts in that pane.

**Rebuild priority.** A `pir:` title is the *authoritative* pane→transcript key —
it was written by the pane itself and survives a session that has exited, so it
needs no inference. It therefore outranks the geometry join, and a pane carrying
an unresolvable title (its log deleted or renamed) is left **empty** rather than
filled by the positional guess. Restoring the wrong conversation into a pane is
the one failure worth more than a pane that starts fresh.

**Rebuilding.** `pir -r @<zellij-session>` recreates that session from its saved
layout, rewriting each `pane command="pir"` to resume its recorded conversation:

```kdl
pane command="pir" cwd="/home/ai/src/pir" {
    args "-r" "pir-20260926-085546-sh945107"
}
```

- `pir -r @vitreous-hill` — rebuild a named session.
- `pir -r @` — rebuild the session this pane is currently in.

The rewrite is a targeted text transform over the dumped KDL, not a parse and
re-print: only `command`/`cwd`/`args` on `pir` panes are touched, and every
other pane, plugin and the `swap_tiled_layout` boilerplate survive
byte-for-byte. Panes with no recorded `pir` (a shell, `htop`, a plugin) are left
exactly as they were, and a pane with no matching record keeps a plain `pir -r`.

When more than one record claims the same pane (the operator resumed another
conversation in place), the freshest `captured` wins.

Implementation notes worth keeping:

- `zellij --new-session-with-layout <file> --session <name>` is what *creates* a
  session. `zellij -s <name>` on its own only *attaches*, and errors if the
  session does not exist; `--layout-string` is for adding a tab to one that does.
- The invocation strips `ZELLIJ`/`ZELLIJ_SESSION_NAME`/`ZELLIJ_PANE_ID` from the
  child environment, or zellij treats it as an action on the *current* session.
- `rebuildable_sessions()` lists the zellij sessions with recorded panes, which
  is what the error path prints when a name does not resolve.

### The create runs detached, and `pir -r @` never hangs

That same invocation **also always attaches** — zellij 0.45.1 has no
`--detach`. So the child process is not "create and return", it is "create and
then become a terminal client", and `Command::output()` on it waits for a human
to detach. With the operator's terminal inherited as a controlling tty, that is
forever: `pir -r @` was observed under gdb sitting in `poll()` inside
`Command::output()`, with the stuck client holding a socket to the new session.

So the create is spawned with `setsid()` and null stdio, waited on with a
deadline, and its success judged by **probing the session**
(`zellij -s <name> action list-panes -a -j`) rather than by the client's exit
code. Three measured facts make this the only correct shape:

| condition | what happens |
|---|---|
| inherits a controlling tty | client attaches, blocks forever — the bug |
| null stdio, no `setsid` | **still hangs** — a controlling tty is what matters, not stdio |
| `setsid()` + null stdio | no controlling terminal → client exits ~101 (`could not enable raw mode`) in ~0.2 s, **after** the server created the session |

The exit-101 panic is cosmetic: the session and its panes live in the zellij
*server*, so they keep running after the client dies (verified — a pane in the
created session was still running five seconds after the client exited). Killing
the client to force a return would be the tempting wrong fix, and it is exactly
what must not happen.

The client is therefore **left alive**, and the session name it created is
printed with the command to reach it:

```
· rebuilt zellij session 'rusty-apricot-3' (1 of 1 recorded pir pane(s) restored)
· attach with: zellij attach rusty-apricot-3
```

### Names: live gets a new one, dead gets reclaimed

zellij refuses a name it holds, in two different ways, and the difference matters
for what the rebuild should do:

- **live** — `Session with name "x" already exists.` (rc 0, on **stdout**). The
  rebuild is resurrecting the session it is sitting in, so it takes the next free
  `name-2`, `name-3`, …
- **exited** — `Session with name "x" already exists, but is dead. …` (rc 1). A
  corpse with nothing in it, so it is `delete-session --force`d first and the
  rebuild then gets **the name the operator asked for**.

`list-sessions --short` cannot tell these apart — it lists exited sessions too —
so the live set comes from `list-sessions -n`, whose lines carry `(EXITED - …)`
inline and are filtered on that. Getting this wrong used to push every rebuild of
an exited session onto an unexplained `-2` suffix.

A regression test drives the real create path under `script(1)`, which is the
only way to reproduce the original condition (a test with null stdio and no
session is the CI case, where the bug never appeared). Removing `setsid()` makes
it fail: **20,032 ms** versus **161 ms** with it.

---

## 16. Multiline pastes collapse in the REPL

Pasting a large block into either REPL no longer scrolls the input zone with
every pasted line. The draft is *displayed* collapsed, while the **full text is
still submitted to the agent**:

```
❯ [ error: bad thing 4 lines pasted ] fix the parser
```

The agent receives all four pasted lines followed by `fix the parser`; only the
echo is abbreviated. The projection lives in `term::collapse_for_display`, which
is pure and never mutates the buffer, so the display and the submission can never
drift apart.

Two details make it behave:

- **The pasted span, not the whole draft.** The input readers record the byte
  range that arrived inside a bracketed-paste wrapper (`ESC[200~ … ESC[201~`) —
  `PasteSpan`, carried on `HistRecall` (streaming REPL) and `PasteState` (TUI).
  Text typed *before* or *after* the paste lies outside that range and stays
  visible, so a paste plus a trailing instruction reads as
  `[ … N lines pasted ] fix the parser` rather than swallowing the instruction.
- **A threshold.** Runs shorter than `PASTE_COLLAPSE_MIN_LINES` (4) are shown
  verbatim — hiding a 3-line paste would be more surprising than echoing it.

This is deliberately **not** done through rustyline's `Highlighter`: rustyline
only consults a highlighter when colours are enabled (so `-n` would silently stop
collapsing), and it computes the cursor layout from the real line length *before*
highlighting, so a shortened echo would leave the terminal cursor past the visible
text.

### The idle prompt: pir reads a paste itself

The idle `❯` prompt is rustyline's, and rustyline echoes a pasted block verbatim —
one terminal line per pasted line — which is the echo the operator actually
watches. There is no supported way to shorten that.

So when a paste is pending, `read_line` **takes the line over** and reads it with
pir's own raw reader (the same `read_chunk` the mid-turn spinner uses), which
collapses the echo while keeping every pasted line in the buffer that is
submitted. The detection is the interesting part:

- raw mode is enabled **before** waiting, because with `ECHO` on the terminal
  paints the paste — wrapper bytes and all (`ESC[200~alpha …`) — the instant it
  arrives, before anything can classify it;
- the wait is a blocking `poll()`, then `ioctl(FIONREAD)` asks how many bytes are
  buffered. `FIONREAD` does not consume, and a paste arrives as one large write,
  so a count above `PASTE_PENDING_BYTES` (16) means a paste while a lone keystroke
  (1–3 bytes) does not;
- a non-paste restores raw mode and returns `None`, and rustyline reads the
  untouched bytes normally — so typing, history and completion are exactly as
  before. A paste is the only case pir owns the line.

`recv(MSG_PEEK)` is the obvious tool here and is **wrong**: on a pty it fails with
`ENOTSOCK` (verified), which is why the check is `poll` + `FIONREAD`. Under
`--no-raw` the takeover is skipped entirely (`raw::enabled()`), since the bracketed
wrapper does not exist on that path.

The projection is rendered at four points: the idle prompt (above), the spinner
line mid-turn, the TUI footer, and the one-line echo at submit.

---

## 13. Environment variables added

| variable | effect |
|---|---|
| `PIR_SCRATCHPAD` | (set, not read) the session's scratch dir, exported to child processes |
| `PIR_SCRATCHPAD_DIR` | root for scratch dirs |
| `PIR_SCRATCHPAD_KEEP` | don't sweep stale scratch dirs |
| `PIR_SCRATCHPAD_TTL_DAYS` | stale-scratch lifetime (default 30) |
| `PIR_ASK_TIMEOUT_SECS` | how long `ask_user_question` waits for an answer (default 120); on timeout the dialog is closed and the result is `TimedOut` |
| `PIR_NO_MODAL` | never use the alternate-screen dialog; use line prompts |
| `PIR_SKILLS_NOSEED` | don't write the bundled skills into the config dir |
| `PIR_PLANS_DIR` | where plan documents live (default `<config>/agent/plans`) |
| `PIR_SUBAGENT` | internal: marks a sub-agent's construction (suppresses launcher-only work) |
| `PIR_SUBAGENT_TIMEOUT_SECS` | sub-agent wall-clock budget (default 600) |
| `PIR_WEB_SEARCH_URL` | search endpoint template containing `{query}` |
| `PIR_WEB_ALLOW` | hosts exempt from the web SSRF address check (`*` for all) |
| `PIR_WEB_NET_TESTS` | run the opt-in live network tests |
| `PI_DIR` | relocates the config dir, and with it the global taste/skills/plans stores |
| `PIR_MCP` | `0` disables MCP entirely (no server is spawned) |
| `PIR_MCP_TIMEOUT_SECS` | per-request MCP timeout (default 120) |
| `PIR_RESOURCE_MEMORY_MIB` | per-command memory budget (overrides the host-derived value) |
| `PIR_RESOURCE_PIDS` | per-command process ceiling (overrides the host-derived value) |
| `PIR_CGROUP` | `0` disables cgroups entirely (detection + host guard only) |
| `PIR_RESOURCE_ENFORCE` | `1` arms kernel enforcement even on a healthy host |
| `ZELLIJ_SESSION_NAME` | (read) recorded per prompt; set by zellij — identifies the session a pir runs in |
| `ZELLIJ_PANE_ID` | (read) recorded per prompt; set by zellij — identifies the pane a pir runs in |

---

## 13. Slash commands added

| command | effect |
|---|---|
| `/scratchpad [ls\|clear]` | show/list/empty the session scratch dir |
| `/skills [name]` | list discovered skills, or print one |
| `/plan [on\|off\|status]` | control plan mode |
| `/plan list` / `/plan show [name]` / `/plan dir` | saved plans |
| `/plan <task>` | enter plan mode with the task pre-filled |
| `/taste [list\|remember\|forget\|find\|path]` | manage recorded preferences |
| `/mcp` | list configured MCP servers and their available tools |
| `/limits` (or `/resources`) | show the resource limits, their basis, and the detected cgroup backend |
| `/checkpoints` | list the rewindable turn boundaries |
| `/rewind <#>` | restore the tree and the conversation to a checkpoint |

`/plan`, `/skills` and `/scratchpad` are all registered in the `match cmd`
dispatch in `src/main.rs`, so tab-completion picks them up automatically (the
completion list is generated from that dispatch by `build.rs`).
