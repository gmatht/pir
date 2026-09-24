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
  120s); a timeout is reported as "dismissed", never as a silent default.

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

---

## 7. Richer file reading

`read_file` now does more than `read_to_string`:

| input | behaviour |
|---|---|
| UTF-8 text | returned as-is |
| gzip (`.gz`, `1f 8b` magic) | decompressed via `gzip -dc`, returned as text |
| PDF (`%PDF-`) | text extracted via `pdftotext` (poppler-utils) |
| Jupyter notebook (`.ipynb`) | rendered as readable cells: each cell's source in a fenced block, its outputs, and its errors — binary outputs (images, PDFs) are named rather than dumped as base64 |
| anything binary | **described** instead of failing: size, detected kind, and a hex/ASCII preview of the first 256 bytes |

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

## 11. Environment variables added

| variable | effect |
|---|---|
| `PIR_SCRATCHPAD` | (set, not read) the session's scratch dir, exported to child processes |
| `PIR_SCRATCHPAD_DIR` | root for scratch dirs |
| `PIR_SCRATCHPAD_KEEP` | don't sweep stale scratch dirs |
| `PIR_SCRATCHPAD_TTL_DAYS` | stale-scratch lifetime (default 30) |
| `PIR_ASK_TIMEOUT_SECS` | how long `ask_user_question` waits for an answer (default 120) |
| `PIR_NO_MODAL` | never use the alternate-screen dialog; use line prompts |
| `PIR_SKILLS_NOSEED` | don't write the bundled skills into the config dir |
| `PIR_PLANS_DIR` | where plan documents live (default `<config>/agent/plans`) |
| `PIR_SUBAGENT` | internal: marks a sub-agent's construction (suppresses launcher-only work) |
| `PIR_SUBAGENT_TIMEOUT_SECS` | sub-agent wall-clock budget (default 600) |
| `PIR_WEB_SEARCH_URL` | search endpoint template containing `{query}` |
| `PIR_WEB_ALLOW` | hosts exempt from the web SSRF address check (`*` for all) |
| `PIR_WEB_NET_TESTS` | run the opt-in live network tests |
| `PI_DIR` | relocates the config dir, and with it the global taste/skills/plans stores |

---

## 12. Slash commands added

| command | effect |
|---|---|
| `/scratchpad [ls\|clear]` | show/list/empty the session scratch dir |
| `/skills [name]` | list discovered skills, or print one |
| `/plan [on\|off\|status]` | control plan mode |
| `/plan list` / `/plan show [name]` / `/plan dir` | saved plans |
| `/plan <task>` | enter plan mode with the task pre-filled |
| `/taste [list\|remember\|forget\|find\|path]` | manage recorded preferences |
| `/checkpoints` | list the rewindable turn boundaries |
| `/rewind <#>` | restore the tree and the conversation to a checkpoint |

`/plan`, `/skills` and `/scratchpad` are all registered in the `match cmd`
dispatch in `src/main.rs`, so tab-completion picks them up automatically (the
completion list is generated from that dispatch by `build.rs`).
