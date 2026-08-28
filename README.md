# pir — a featherweight `pi`-compatible coding agent (in Rust)

`pir` ("**p**i in **R**ust") is a minimal, fully-synchronous terminal coding
agent that reuses your existing [`~/.pi`](https://github.com/) setup
**read-only** and adds a few power-user features: persistent goals, resumable
sessions, background jobs, and per-project sandbox users.

It's a single static binary, three runtime dependencies (`ureq`, `serde`,
`serde_json`), no async runtime (no tokio), and it speaks both wire formats
that `pi`'s providers use — the **Anthropic Messages API** and the
**OpenAI-compatible chat completions API** (incl. OpenRouter / DeepSeek / etc.).

---

## Features

- **Drop-in config reuse** — reads `~/.pi/models.json`, `~/.pi/agent/settings.json`,
  `~/.pi/AGENTS.md` and `./AGENTS.md`, and writes transcripts under
  `~/.pi/agent/sessions/`. Your `pi` setup is never modified.
- **Streaming** token output over SSE for both provider APIs.
- **Five built-in tools**: `bash`, `read_file`, `write_file`, `edit_file`, `list_dir`,
  with `y`/`a`/`n` confirmation prompts (or `-y` full-auto).
- **Extensions** — drop a folder in `extensions/<name>/src/lib.rs` exporting
  `register()` and it is statically linked into the binary at compile time
  (no runtime loader, no `Cargo.toml`). See [`src/plugin.rs`](src/plugin.rs).
  The `wt` extension (per-agent git worktrees, **on by default**) and
  `autocommit` (commit after every prompt) are included.
- **Resumable sessions** — `pir -r` reloads a past session (by index / time /
  preview) and keeps the conversation going.
- **Goals** — a durable `<session>.goal.json` tracks multi-step objectives;
  `pir -c` resumes a session *and* drives it to the next pending step, surviving
  ctrl-c, crashes, or timeouts. See [`src/goal.rs`](src/goal.rs).
- **Background jobs** — run prompts with `&` or `/bg` (or `pir -bg`); they run
  on worker threads and notify you on completion. `/jobs` lists, `/fg <id>`
  foregrounds.
- **Per-project sandbox users** — `pir project init` (as root) creates a
  non-login `ai_<project>` user owning the cwd, so every tool executes as that
  identity. `ai_*` users run unattended (full-auto) by default; override with
  `--confirm` / `PI_CONFIRM=1`.

---

## Build & run

```bash
cargo build --release

export ANTHROPIC_API_KEY=sk-...      # or whatever your ~/.pi/models.json references

./target/release/pir                    # REPL
./target/release/pir -m sonnet          # fuzzy model match
./target/release/pir -y "fix the TODO in src/lib.rs"   # one-shot, full-auto
echo "explain this repo" | ./target/release/pir        # piped one-shot
```

### Resume / continue / background

```bash
pir -r                 # resume latest session from this shell
pir -r 3               # resume session index 3 (see `pir -r` list)
pir -c                 # resume + continue the goal attached to that session
pir -bg "refactor the parser"   # run entirely in the background
# Inside the REPL:
> fix the flaky test &    # ends with & => background job
> /bg write the docs      # explicit background
> /jobs                   # list background jobs
> /fg 1                   # foreground job #1 (reload its session)
```

### Per-project users

```bash
sudo ./target/release/pir project init      # creates ai_<project>, chowns cwd
sudo -u ai_<project> ./target/release/pir    # ...then run as that user
```

`pir project init` (run as **root**) also gives the new `ai_<project>` user its
own, self-owned **network-capable toolchain directories**, so the agent can do
its job without touching root's files:

- `~ai_<project>/.cargo` — a writable `CARGO_HOME` (the default `/root/.cargo`
  is `0700` root, so an unprivileged agent can't write its registry cache).
- `~ai_<project>/.config/gh` — a writable `GH_CONFIG_DIR` (preferred over
  widening traversal on root's `~/.config/gh`; see `ai-permctl`).

When `pir` drops privileges to `ai_<project>` (via `become_user`), it exports
`CARGO_HOME` / `GH_CONFIG_DIR` pointing at those dirs (resolved by
`user::toolchain_env_for`). Net effect: **`ai_*` agents have outbound network
access** (fetch crates, call APIs, push to GitHub) but own none of root's
files. The `ai-permctl` read grant remains the only path to root's `gh` config.

---

## Usage

```
pir [options] [prompt]     prompt given => one-shot, else interactive REPL
pir -r [token] [prompt]    resume a session (latest from this shell by default)
pir -c [token] [prompt]    continue a goal: resume a session + drive its next step
pir -bg <prompt>           run a prompt entirely in the background (notifies on done)

OPTIONS
  -m, --model <selector>     e.g. -m anthropic/claude-sonnet-4-5 (fuzzy match ok)
  -y, --full-auto            no confirmation for shell/write tools
  --confirm                  always prompt to confirm shell/write tools
  -n, --no-color             disable ANSI colors
  -r, --resume [token]       resume a session; token selects by index/time/preview
  -c, --continue [token]     resume a session and continue its goal (pir -c)
  -u, --as <user>            run project commands as this user (default ai_<project>)
  -h, --help  -V, --version

COMMANDS (REPL)
  /help  /model <sel>  /models  /sessions  /goal [objective]  /continue
  /bg <text>  /jobs  /fg <id>  /clear  /usage  /exit
  /thinking [<level>] [show|hide]   set the model's thinking level
                                    (off|minimal|low|medium|high|xhigh|max) and/or
                                    toggle whether reasoning is displayed
  /project init            create the ai_<project> user and chown the cwd (root)
  /su-security <on|off|status>  enable/disable/inspect the su-based permission model (root)
  /create [name]           scaffold a new project (seeds from clipboard .md spec)
```

A line ending in `&` runs in the background (`fix the parser &` ⇒ `/bg fix the parser`).

---

## Configuration

`pir` reads `pi`'s config and never writes over it:

| Path | Purpose |
|------|---------|
| `~/.pi/models.json` | providers, base URLs, models, API keys (`{env:VAR}` expansion supported). A starter file is written if missing. |
| `~/.pi/agent/settings.json` | optional default model (`"model"` key). |
| `~/.pi/AGENTS.md`, `./AGENTS.md` | appended to the system prompt. |
| `~/.pi/agent/sessions/` | `pir-*.jsonl` session transcripts + `<session>.goal.json` goal files. |
| `~/.pi/agent/projects.json` | project → execution-user mappings (set by `pir project init`). |

Model selection accepts `provider/model`, a bare model id, or a fuzzy substring
(match is case-insensitive against `provider/model` and display name).

### Environment variables

| Var | Effect |
|-----|--------|
| `PI_MODEL` | default model selector |
| `PI_DIR` | override the `~/.pi` config directory |
| `PIR_PROJECTS_DIR` | base dir for `/create` (default `~/.pi/projects`) |
| `PI_FULL_AUTO` | force full-auto (no confirmations) |
| `PI_CONFIRM force confirmation prompts (even as an `ai_*` user) |
| `NO_COLOR` | disable |
| `PIR_WT` | `0` disables per-agent worktree automation (on by default); `wt` tool is always available |
| `PIR_WT_AUTO` | `0` disables auto-verify/merge on idle (worktrees still created) |
| `PIR_WT_CHECK` | explicit build/test verify command for `wt` auto-merge |
| `PIR_WT_DIR` | override worktree parent dir (default `<repo>/.git/wt`) |

---

## Project layout

```
pir/
├── Cargo.toml
├── build.rs                 # scans extensions/*/src/lib.rs -> generated registry
├── deploy.sh                # build + test + install gate (see below)
├── install-skynet-ai.sh     # optional: deploy the ai_* permission model (root)
├── SKYNET-AI-PERMS.md       # design notes for the permission model
├── .gitignore               # merged Rust/editor/OS + /.pir ignores
├── .gitwhitelist            # source/doc/script patterns always tracked
├── scripts/
│   └── git-wl-add           # force-add files matching .gitwhitelist
├── unmd.sh / unmd2.sh       # extract markdown file specs into a project tree
├── extensions/
│   ├── builtin/             # example statically-linked extension
│   └── autocommit/          # optional: commit after every prompt (off by default)
└── src/
    ├── main.rs      # CLI + REPL + background jobs + project commands
    ├── agent.rs     # the agent loop + session load/save + goal driving
    ├── config.rs    # ~/.pi loading + model selection
    ├── types.rs     # internal message model (Anthropic <-> OpenAI mapping)
    ├── provider.rs  # HTTP + SSE for both APIs
    ├── plugin.rs    # tool backend ABI + registry (extensions)
    ├── goal.rs      # persistent goal/step framework
    ├── user.rs      # per-project ai_<project> sandbox user (unix)
    ├── project.rs   # /create + clipboard markdown scaffolding
    ├── notify.rs    # desktop/terminal notifications on completion
    └── term.rs      # colors, dates, prompts, shell-pid helpers
```

---

## Writing an extension

`build.rs` automatically links every `extensions/<name>/src/lib.rs` that exports
`pub fn register(reg: &mut pir::plugin::Registry)`. Rebuild and the tool appears
in the model's tool list — no `Cargo.toml`, no runtime loader.

```rust,ignore
use crate::plugin::{Outcome, Registry, ToolBackend, ToolSpec};
use serde_json::json;

pub fn register(reg: &mut Registry) {
    reg.add(Box::new(MyExt));
}

struct MyExt;
impl ToolBackend for MyExt {
    fn name(&self) -> &'static str { "my-ext" }
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "hello",
            description: "say hello",
            schema: json!({ "type": "object", "properties": {}, "required": [] }),
        }]
    }
    fn run(&mut self, name: &str, _input: &serde_json::Value) -> Outcome {
        match name {
            "hello" => Outcome::ok("hello".into()),
            other => Outcome::err(format!("unknown tool '{other}'")),
        }
    }
}
```

Because extensions are compiled into `pir`, they can call any `crate::*` module
(`crate::term`, `crate::config`, …). See [`src/plugin.rs`](src/plugin.rs) for
the full ABI.

---

## Ignore / whitelist (`git`)

The repo uses two complementary files so source is never accidentally dropped
while build/agent junk is never committed:

- **`.gitignore`** — merges the standard **Rust** template (`/target`,
  `*.rs.bk`; `Cargo.lock` is kept on purpose for reproducible binary builds),
  common **editor/IDE** ignores (vim, emacs, VS Code, JetBrains), **OS** files
  (`.DS_Store`, `Thumbs.db`, …), and the pir-specific **`/.pir`** per-project
  agent metadata dir (session transcripts / history — local, not shared).
- **`.gitwhitelist`** — the project-defining files that must *always* be
  tracked even if `.gitignore` would exclude them: `src/**/*.rs` (every `.rs`
  under `src/`, including `extensions/*/src/`), repo-root `*.rs` (`build.rs`),
  `Cargo.toml`, `Cargo.lock`, `*.md` + `LICENSE.GPL3`, `*.sh` (incl.
  `deploy.sh` / `install-skynet-ai.sh`), and the ignore/whitelist files
  themselves.

`scripts/git-wl-add` force-adds any untracked file matching `.gitwhitelist`
(overriding `.gitignore`), idempotently — it only ever `git add`s (no commit /
push) and never force-adds build artifacts. Run it after creating new source,
docs, or scripts so they can't be left out:

```sh
./scripts/git-wl-add --dry-run   # show what would be added
./scripts/git-wl-add             # force-add matching untracked files
```

---

## Optional: `ai_*` permission model

`install-skynet-ai.sh` (run as **root**) deploys a permission model where an
orchestrator user (`skynet` / `skynet_*`) may passwordlessly `su` to and create
`ai_<alnum>+` accounts, any user `X` may `su` to its own `X__<alnum>+` underlings,
and `ai_*` users may run a logged, validated `apt install`. Each permission is
prompted yes/no; root-owned wrappers in `/usr/local/sbin/` are the authoritative
boundary and a `visudo`-validated `/etc/sudoers.d/skynet-ai` is emitted. See
[`SKYNET-AI-PERMS.md`](SKYNET-AI-PERMS.md) for the design.

---

## Honest caveats

- **Schema tolerance**: the loader accepts `providers` as a list (pi's format)
  or map, camelCase/snake_case keys, and `{env:...}` keys — but it isn't
  guaranteed against every `pi` version's `models.json`. If yours differs,
  `config.rs` names the exact file in its error.
- Ctrl-C mid-stream kills the process (no raw-terminal mode); the JSONL log and
  `.goal.json` preserve everything up to the last completed message, and `pir -c`
  can pick up where it left off.
- No markdown rendering, no parallel in-loop tool execution, no sub-agents, no
  MCP — that's the "lightweight" deal. Adding a built-in tool is one
  `ToolSpec` + one match arm; adding an extension is one folder.
- `-y` runs arbitrary shell commands bounded only by a 120s timeout; the default
  confirmation mode is the sane choice. When running as an `ai_*` user, `pir`
  defaults to full-auto since the user account is the sandbox boundary.
- Needs `rustc ≥ 1.70` (`IsTerminal`); Windows builds but ANSI support depends
  on your terminal, and per-project users / `become_user` are unix-only.
