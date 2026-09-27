# pir vs Command Code: Feature Comparison

**Status:** living document — updated as pir evolves
**Scope:** side-by-side feature comparison, gap analysis, and headless-worker feature proposals.

---

## 1. Executive Summary

pir is a ~4 MB Rust binary using ~1 MB RSS at idle. Command Code is a ~260 MB
Node.js application using ~164 MB RSS. Both are terminal coding agents with
tool use, session management, and multi-model support.

pir covers the core coding-agent loop well. The biggest gaps are in
**extensibility** (MCP, skills, mods, hooks), **structured planning**, and
**rich content handling**. For headless/worker use, pir's lightweight footprint
is a major advantage but it lacks several automation-oriented features.

### Recently implemented (headless-worker features)

- **Structured exit codes** — `ExitCode` enum with numeric values: 0=success,
  1=general, 3=auth, 4=permission, 5=rate-limit, 6=network, 7=server-error,
  9=no-response, 130=interrupted. The one-shot path (`pir "prompt"`) now exits
  with the classified code instead of always returning 0.

- **NDJSON output mode** — `--output-format json` emits structured events to
  stdout: `turn_start`, `tool_call`, `tool_result`, `turn_end`, `result` (with
  exit code, full text, and token counts). Suppresses terminal output. Designed
  for headless / CI use.

- **Native search tools** — `glob` (path globs, newest-first, `.gitignore`-aware)
  and `grep` (regex content search with `content` / `files_with_matches` /
  `count` modes, context lines, `-i`, glob/type filters). ripgrep-backed with an
  in-process fallback; both paginate and say exactly what was cut.

- **Session scratchpad** — a per-session writable dir outside the repo, exported
  to every child process as `$PIR_SCRATCHPAD`; 30-day lazy sweep of abandoned
  dirs; `/scratchpad` to inspect.

- **`ask_user_question`** — the model can pause a turn and ask a structured
  question (header, body, options, multi-select) and resume with the answer.
  Interactive picker, line-prompt fallback, and an explicit "no operator
  attached" result when headless — it never hangs.

- **Skills** — file-based, progressively-disclosed instruction sets discovered in
  `.pir/skills` and `~/.pi/skills`. Only names + descriptions are in the prompt;
  bodies load on demand via `load_skill`. Ships a `locate` skill.

- **Plan mode** — `SecurityLevel::Plan` + `plan_write`: the agent explores
  read-only, writes a plan document, and waits for approval before touching
  anything. Persisted per session; `/plan` to control it.

- **Sub-agent delegation** — the `agent` tool spawns a child with its own context
  window (`explore` / `plan` / `general`), returns only its report, never
  widens the security posture, and has a wall-clock deadline so a wedged child
  can't hold the parent.

- **Richer `read_file`** — transparent gzip decompression, PDF text extraction
  via `pdftotext`, and a described hex/ASCII preview for binary files instead of
  a UTF-8 error.

- **Provider-independent web tools** — `web_fetch` / `web_search` over the
  runtime-loaded host libcurl, with an SSRF guard (no loopback / private /
  link-local / cloud-metadata), HTML→text reduction, and a pluggable search
  backend (`PIR_WEB_SEARCH_URL`, SearXNG JSON or DuckDuckGo HTML).

- **Taste (recorded preferences)** — the `remember` tool records durable
  preferences as they come up (with evidence + confidence), persisted in
  `.pir/taste/` (project) and `~/.pi/taste/` (user) and injected into every
  later session's system prompt. Re-recording reinforces rather than
  duplicates; `/taste` manages them.

- **Session checkpoints and rewind** — a checkpoint opens at every turn,
  capturing each file the turn touches (once, before its first edit) plus the
  conversation length. `/rewind <#>` restores the working tree *and* truncates
  the conversation, so the model stops building on undone work; it persists
  across a crash or resume. Shell side effects are explicitly out of scope and
  the result says so.

- **MCP servers (stdio)** — tools from Model Context Protocol servers appear
  alongside the built-ins, namespaced `mcp__<server>__<tool>`. Configured in
  `~/.pi/agent/mcp.json`; a server that fails to start is reported once in the
  banner and ignored rather than being fatal, and every request is timeout-bounded
  so a wedged server cannot hang a turn. `/mcp` lists what is available.

See [FEATURES.md](FEATURES.md) for the full documentation of these.

---

## 2. Resource Usage

| Metric | pir | Command Code |
|---|---|---|
| Binary + runtime | 3.8 MB | 118 MB (node) + 260 MB (package) |
| RSS (idle) | ~1 MB | ~164 MB |
| Virtual memory | ~3 MB | ~9.6 GB |
| Threads (idle) | 1 | 9 |
| Language | Rust | TypeScript / Node.js |

pir is ~160x lighter at idle. Both are negligible on modern hardware, but the
difference matters when running many concurrent agent instances.

---

## 3. Feature Matrix

### 3.1 Core REPL

| Feature | pir | cmd |
|---|---|---|
| Interactive REPL | yes | yes |
| One-shot mode | `pir "prompt"` | `cmd "prompt"` |
| Background mode | `pir -bg` | — |
| Streaming output | yes | yes |
| Markdown rendering | yes (synoptic) | yes |
| Syntax highlighting | yes | yes |
| Tab completion | yes | yes |
| Inline hints (ghost text) | yes | — |
| Multiple frontends | streaming, TUI, GTK, curses | streaming only |
| Mid-turn type-ahead | yes | yes |
| Mid-turn backgrounding (`&`) | yes | — |
| Suspend (Ctrl-Z) | yes | — |

### 3.2 Session Management

| Feature | pir | cmd |
|---|---|---|
| Session persistence | JSONL | JSONL |
| Resume session | `pir -r` | `cmd -r` |
| Continue/goal mode | `pir -c` | `cmd -c` |
| Session picker | yes | yes |
| Session naming | auto (light model) | manual `/rename` |
| Session forking | — | `/fork`, `/clone` |
| Session tree | — | `/tree` |
| Checkpoint/rewind | — | per-prompt checkpoints |
| Sharing/export | — | `/share`, `/export`, gist |
| Scratchpad directory | — | per-session temp dir |

### 3.3 Tools

| Tool | pir | cmd |
|---|---|---|
| Shell execution | `bash` | `shell_command` |
| Read file | `read_file` (UTF-8) | `read_file` (UTF-8, images, PDF, notebooks, Office) |
| Write file | `write_file` | `write_file` |
| Edit file | `edit_file` (single replacement) | `edit_file` (multi-replacement, 6-strategy match) |
| List directory | `list_dir` | `read_directory` |
| Glob | via bash | `glob` tool |
| Grep | via bash (ripgrep alias) | `grep` tool (ripgrep) |
| Web search | ollama-cloud ext only | built-in for all models |
| Web fetch | ollama-cloud ext only | built-in for all models |
| Background job mgmt | `job_status`, `job_kill` | `shell_output`, `shell_tasks`, `kill_shell` |
| Sub-agents | — | `agent`, `agent_output` (general, explore, plan) |
| Ask user | — | `ask_user_question` (structured options) |
| Plan mode | — | `enter_plan_mode`, `plan_review` |
| Worktrees | `wt_*` tools (ext) | `enter_worktree`, `exit_worktree` |
| Goal management | `update_goal` | — |
| Stop signal | `request_stop` | — |

### 3.4 Extensibility

| Feature | pir | cmd |
|---|---|---|
| MCP servers | — | HTTP + stdio, OAuth, 3 scopes |
| Skills | — | full progressive-disclosure system |
| Custom agents | — | `.md` frontmatter files |
| Mods (plugins) | — | TypeScript ModApi |
| Hooks | extension trait (compiled) | user-configurable Pre/PostToolUse, Stop, SessionStart |
| Taste learning | — | continuous learning + push/pull sync |

### 3.5 Model & Provider Support

| Feature | pir | cmd |
|---|---|---|
| Multi-provider | yes | yes |
| OpenAI-compatible | yes | yes |
| Anthropic wire | yes | yes |
| OpenAI Responses API | yes | yes |
| BYOK / custom providers | yes (`models-store.json`) | yes (`providers.json`, `/connect`) |
| Model fuzzy matching | yes | yes |
| Cross-instance broadcast | `/model*` | — |
| Reasoning/thinking levels | yes (7 levels) | yes |
| Thinking format variants | deepseek, openrouter, qwen, openai | auto-detected |
| Token budget cap | `--budget` | — |
| Cost tracking | yes | yes (`/usage`) |
| Light model (titles) | yes (Cerebras default) | yes |

**Space Bunny Alpha routes in pir**

Space Bunny Alpha is a free-preview, OpenAI-compatible model with a 1,000,000-token
context window, a 524,288-token completion ceiling, mandatory reasoning, and tool
support. When the corresponding credential is present, pir exposes both routes
separately:

| Route | Provider ID | Model ID | Endpoint | Credential |
|---|---|---|---|---|
| OpenRouter | `openrouter` | `stealth/space-bunny-alpha` | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` or `auth.json` entry `openrouter` |
| Command Code Provider API | `commandcode` | `stealth/space-bunny-alpha` | `https://api.commandcode.ai/provider/v1` | `CMD_API_KEY` (or `COMMANDCODE_API_KEY`) or `auth.json` entry `commandcode` |

Select either route with its qualified label, for example
`pir -m openrouter/stealth/space-bunny-alpha` or
`pir -m commandcode/stealth/space-bunny-alpha`. OpenRouter receives the nested
`reasoning: {effort}` field; the Command Code route receives the standard
`reasoning_effort` field. The preview is free while available, but provider
eligibility, rate limits, and retention terms still apply.

### 3.6 Security & Sandboxing

| Feature | pir | cmd |
|---|---|---|
| Security levels | 6 levels (sandbox→off) | permission modes (default→yolo) |
| Per-project sandbox users | yes (`ai_<project>`) | — |
| Write quarantine (overlayfs) | yes | — |
| Command mitigation engine | yes | — |
| Privilege escalation (rootreq) | yes | — |
| Secret scanning on writes | — | yes |
| Workspace boundary | via security levels | yes |

### 3.7 Notifications

| Feature | pir | cmd |
|---|---|---|
| Terminal bell | yes | — |
| Desktop notifications | yes | — |
| Sound | yes | — |
| Webhook | yes | — |
| File stamp | yes | — |

### 3.8 Git Integration

| Feature | pir | cmd |
|---|---|---|
| Worktree isolation | yes (ext, with PR submit) | yes (managed) |
| Autocommit | yes (git + jj) | — |
| Commit guard hook | yes | — |
| jj (jujutsu) support | yes | — |

---

## 4. What pir Has That cmd Doesn't

- **Security sandboxing** — per-project Unix users, overlayfs quarantine, command mitigation engine
- **Multiple REPL frontends** — TUI (ratatui), GTK, curses, streaming
- **Background mode** (`pir -bg`) — fully detached agent runs
- **Cross-instance model broadcast** — switch model in one terminal, all others follow
- **Mid-turn backgrounding** — `&` detaches a running turn
- **Privilege escalation requests** — auditable rootreq for apt-install, user creation
- **Autocommit** — automatic git/jj commits after turns
- **Goal continuation framework** — structured multi-step objectives with light-model backstop
- **Notification bus** — bell, desktop, sound, webhook, file stamp
- **Token budget cap** — hard stop at cumulative token limit
- **Inline hints** — ghost-text preview as you type
- **Sixel logo** — startup banner in supported terminals
- **Death provenance tracking** — signal handler logs what killed the process
- **Config backup** — automatic `~/.pi_backup.tgz` at startup

---

## 5. What cmd Has That pir Doesn't

### High impact

- **MCP** — connect to any Model Context Protocol server as tools
- **Skills** — installable, portable agent skill ecosystem
- **Custom agents** — user-defined sub-agents with own tool sets and models
- **Hooks** — user-configurable PreToolUse/PostToolUse/Stop/SessionStart
- **Mods** — TypeScript loadable plugins (ModApi)
- **Plan mode with review UI** — structured plan-draft → inline-comment → approve workflow
- **Sub-agents** — `agent` tool with explore/plan/general types, parallel execution

### Medium impact

- **`glob` and `grep` as tools** — model-callable without bash
- **Rich file reading** — images, PDFs, notebooks, Office docs
- **Session forking/branching/tree** — checkpoint-based conversation branching
- **`ask_user_question`** — structured multiple-choice prompts
- **`sleep` and cron scheduling** — time-based task scheduling
- **Web tools for all models** — not gated behind an extension
- **IDE integration** — VSCode open-file context, LSP diagnostics
- **Session sharing** — live links, GitHub gists

### Lower impact

- **Taste learning** — continuous preference learning
- **Headless exit codes** — structured (3=auth, 4=perm, 5=rate-limit, etc.)
- **`/context` command** — context window breakdown visualization
- **Decision models** — probability-based typed answers (`typesafe/jev`)
- **Session scratchpad** — per-session temp dir shared with sub-agents

---

## 6. Headless Worker Feature Proposals

pir already runs headless (`pir "prompt"`, `pir -bg`, `PIR_HEADLESS=1`). These
proposals target the use case of pir as a worker in CI, batch pipelines, and
multi-agent orchestration systems.

### 6.1 Structured exit codes

**What:** Map exit codes to machine-readable outcomes (like cmd's scheme).

```
0  success
1  general error
2  tool failure
3  auth failure
4  permission denied
5  rate limit
6  network error
7  API server error (5xx)
8  max turns / budget exceeded
9  model produced no response
130 interrupted (SIGINT/SIGTERM)
```

**Why:** Workers need to distinguish "retry this" from "give up" without
parsing stderr. Currently pir exits 1 for most failures.

### 6.2 JSON/NDJSON output mode

**What:** `--output-format json` emits one NDJSON event per turn:
`{"type":"turn","tokens_in":N,"tokens_out":N,"cost":0.003,"duration_ms":1200}`
Final line: `{"type":"result","exit":0,"text":"..."}`.

**Why:** Batch orchestrators need structured output. Currently pir's output is
human-readable text mixed with debug lines.

### 6.3 Headless tool allowlist/denylist

**What:** `--tools-allow bash,read_file` / `--tools-deny write_file` flags.
In headless mode, only whitelisted tools are registered with the model.

**Why:** CI workers often need read + shell but not write. Currently security
levels are coarse; this is finer-grained.

### 6.4 Stdin prompt mode

**What:** `pir --stdin` reads the full prompt from stdin (multi-line) instead
of requiring it as an argument. Supports heredocs and pipes cleanly.

**Why:** Long prompts (file contents, diffs) are awkward as shell arguments.
Piping currently works but pir's stdin detection is fragile.

### 6.5 Sub-agent delegation

**What:** A built-in `agent` tool that spawns a child pir process with its own
context window and tool set. Two types: `explore` (read-only) and `general`
(all tools). Child runs in background, parent collects result via `agent_output`.

**Why:** Complex tasks benefit from parallel exploration. Currently pir's
single-context model means one long conversation. Sub-agents would let the
model delegate research while continuing implementation.

### 6.6 Web tools as built-in

**What:** Promote `web_search` and `web_fetch` from ollama-cloud extension to
core tools (gated behind `--web-tools` flag or config).

**Why:** Workers often need to fetch docs, check APIs, or search for error
solutions. Currently requires ollama-cloud provider.

### 6.7 Glob and grep as tools

**What:** Register `glob` and `grep` as direct tools instead of relying on
bash aliases.

**Why:** Dedicated tools give the model structured output (file lists with
metadata, matches with context). Bash `rg` output requires the model to parse
text. Also: tools can enforce workspace boundaries; bash cannot.

### 6.8 Cron / scheduled prompts

**What:** `pir --cron "0 */6 * * *" "run tests and report"` schedules recurring
prompts. Uses a lightweight in-process timer (no system cron dependency).

**Why:** Monitoring workers that check builds, poll deploys, or watch PRs on
a schedule. Currently requires external cron wrapping `pir "prompt"`.

### 6.9 Session hooks (pre/post turn)

**What:** User-configurable hooks in `~/.pi/agent/hooks.toml`:
```toml
[pre_turn]
run = "git pull --rebase"

[post_turn]
run = "cargo check 2>&1 | tail -20"

[on_error]
run = "notify-send 'pir worker failed'"
```

**Why:** Workers need deterministic pre/post actions (pull before, check after,
alert on failure). Currently requires wrapping pir in a shell script.

### 6.10 Structured tool-call logging

**What:** `--tool-log <path>` appends one JSON line per tool call:
```json
{"ts":"...","tool":"bash","args":{"command":"cargo test"},"exit":0,"duration_ms":3200}
```

**Why:** Audit trails for unattended agents. Currently tool calls are buried in
the session JSONL and hard to extract programmatically.

### 6.11 Retry policy configuration

**What:** Configurable retry behavior in `settings.json`:
```json
{
  "retry": {
    "maxAttempts": 3,
    "onVerdicts": ["retry", "error"],
    "backoffMs": [1000, 5000, 30000]
  }
}
```

**Why:** Workers need predictable retry behavior. Currently pir's retry is
model-judged (light model verdicts) with no user control over attempt count or
backoff.

### 6.12 Health check endpoint

**What:** `pir --health-port 9090` starts a minimal HTTP server exposing:
`GET /health` → `{"status":"ok","model":"...","session":"...","uptime_s":123}`
`GET /metrics` → prometheus-format token/cost/turn counters.

**Why:** Orchestrators (k8s, docker-compose) need liveness probes. Currently
no way to check if a pir worker is alive without parsing its output.

### 6.13 Batch file processing

**What:** `pir --batch tasks.txt` where each line is a prompt. Runs them
sequentially (or `--batch-parallel N` concurrently), writes results to
`tasks.out/` as individual files.

**Why:** Processing a queue of independent tasks (code reviews, migrations,
test generation) without scripting a loop.

### 6.14 Resume-aware CI integration

**What:** `pir --ci` mode that:
- Writes a machine-readable `.pir-result.json` on exit
- Emits GitHub Actions annotations (`::error::`, `::warning::`) for tool failures
- Sets `PIR_SESSION_ID` env var for downstream steps to resume

**Why:** CI pipelines need structured integration, not just "run pir and hope".

---

## 7. Recommended Priority

For headless workers, implement in this order:

1. **Structured exit codes** (6.1) — trivial, highest ROI for automation
2. **JSON output mode** (6.2) — enables every downstream consumer
3. **Glob/grep as tools** (6.7) — better model behavior, workspace boundaries
4. **Web tools built-in** (6.6) — removes extension dependency
5. **Tool-call logging** (6.10) — audit trail for compliance
6. **Headless tool allowlist** (6.3) — fine-grained security
7. **Sub-agent delegation** (6.5) — parallel task execution
8. **Session hooks** (6.9) — deterministic pre/post actions
9. **Retry policy** (6.11) — predictable failure handling

The remaining proposals (6.4, 6.8, 6.12, 6.13, 6.14) are valuable but can
wait until the foundation is solid.
