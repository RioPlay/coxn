# coxn

A lean, provider-agnostic terminal harness for running a coding LLM with a
human approval gate and a bwrap sandbox. Zero aden required; aden plugs in
optionally to amplify with structure-aware context and a blast-radius scope gate.

`coxn` is a *dumb pump*: it steers and sets pace and carries no intelligence.
The split:

- **the LLM** acts: it reads, reasons, edits, and runs commands.
- **coxn** steers: it runs the loop, dispatches tools, gates every action, and
  renders the TUI. Nothing more.
- **aden** (optional) amplifies: it adds structure-aware search, blast-radius
  scope checking, and task partitioning -- none of which coxn requires to function.

The name is coxswain shorthand (`cox'n`): the cox steers the boat and calls the
cadence but never rows. See `DESIGN.adoc` for the full design and laws.

## Why coxn is different

coxn sits behind three independent gates, each of which can stop an action the
others permit. An edit that the model wants, that you approve, and that compiles
still does not land if it escapes the scope:

- **Human gate** — every mutating tool call asks for approval before it runs.
  `run_command` approval is scoped to the *exact command string*, so a
  near-identical command re-prompts.
- **OS gate** — `run_command` runs inside a `bwrap` namespace with no network by
  default and the environment scrubbed (API keys never leak in). The prompt
  labels `[sandbox]` vs `[NO SANDBOX]` honestly.
- **Blast-radius gate** — with a task scope active, coxn applies an edit, runs
  `aden impact-diff --scope`, and **reverts the file on a scope-escape** before
  its result is accepted. A change outside the manifest's file mandate never
  persists, even if the model made it and you approved it.

The wedge is visible in one minute:

```sh
bash scripts/demo-scope-escape.sh
```

It builds a throwaway repo, shows an in-scope edit allowed, an out-of-scope edit
blocked and reverted, and the sandbox state. No cloud keys required.

Record for docs (requires [asciinema](https://asciinema.org/) or [vhs](https://github.com/charmbracelet/vhs)):

```sh
bash scripts/record-scope-escape.sh
```

### Scoped edits and a dirty git tree

When `COXN_TASK_NAME` is set, aden's gate compares the **entire working tree**
against `HEAD`, not just the file being edited. Uncommitted changes outside the
task's file mandate can block every scoped edit until you commit or stash them.
`coxn doctor` and `/scope` warn when a scope is active on a dirty tree.

### aden caveats (when scoped)

aden's blast-radius gate is strong on free-function Rust (~0.99 recall) but weaker on
object-oriented / method-dispatch code (~0.61 recall on OO fixtures). Treat scoped edits
in OO codebases conservatively until method-call resolution improves upstream.

On prose-heavy repos, index vs graph anchor namespaces can degrade `aden ask`; prefer
`grep`, `asm`, or `understand` for structural context. `coxn doctor` prints these notes
when `COXN_TASK_NAME` is set and aden is on PATH.

## Prerequisites

1. **A model** — Ollama/LM Studio running locally, or `COXN_MODEL_BASE_URL` + `COXN_MODEL_KEY`
2. **Optional:** `bwrap` for sandboxed `run_command` (Linux)
3. **Optional:** `aden` ≥ 0.2.0 for blast-radius scope gate

```sh
coxn doctor   # one-screen health check
```

Install: see [INSTALL.md](INSTALL.md). `cargo install --path .` from this repo.

## Quickstart

### Standalone (no aden needed)

```sh
# Auto-detect a local model server (Ollama :11434 or LM Studio :1234):
cargo run

# Point at any OpenAI-compatible endpoint:
COXN_MODEL_BASE_URL=http://localhost:1234/v1 COXN_MODEL_NAME=my-model cargo run

# Against OpenRouter / OpenAI:
COXN_MODEL_BASE_URL=https://openrouter.ai/api/v1 \
  COXN_MODEL_NAME=openai/gpt-4o \
  COXN_MODEL_KEY=sk-... \
  cargo run
```

With nothing running, coxn falls back to an offline stub so the TUI still comes
up.

### Amplified (aden on PATH)

```sh
# Aden provides structured context; set a task scope for the blast-radius gate:
COXN_TASK_NAME=fix-parser \
  COXN_TASK_SEEDS=parse_config \
  cargo run
```

The status line shows `scope: fix-parser` when the gate is active, or `ungated`
when aden is absent or no task is set.

## Choosing a model

Resolution order:

1. `COXN_MODEL_BASE_URL` (+ optional `COXN_MODEL_NAME`, `COXN_MODEL_KEY`) in env
2. `.aden/config.toml` provider profiles: `[provider.*]` plus `[route].active`
3. legacy `aden config` values (`model.base_url`, `model.name`) -- only when aden is present
4. logged-in CLIs on PATH (grok → claude → codex), then native Ollama (`:11434`)
5. HTTP auto-detect (LM Studio `:1234`, Ollama OpenAI-compat)
6. offline stub (use Ctrl-Space → **setup** presets, or `/auth setup <id>`)

**First run:** `coxn` → Ctrl-Space → pick `setup grok-cli`, `setup ollama-native`, or
`setup openrouter-claude`. Setup hot-reloads the model immediately; cloud keys paste
via `/auth set-key <instance>`.

Any OpenAI-compatible endpoint works: LM Studio, Ollama, vLLM, OpenRouter,
OpenAI. Secrets come from environment variables or `~/.config/coxn/secrets/`,
never a committed config file.

Example `.aden/config.toml`:

```toml
[provider.local]
driver = "openai_compat"
base_url = "http://localhost:11434/v1"
enabled = true

[provider.openrouter]
driver = "openai_compat"
base_url = "https://openrouter.ai/api/v1"
enabled = false

[route]
active = "local:qwen2.5-coder"
scout = "local:qwen2.5-coder"
synth = "openrouter:anthropic/claude-sonnet-4"
```

Cloud instances are never used accidentally. Enable the provider in config and
set a key such as `COXN_KEY_OPENROUTER`; without a key, set `COXN_ALLOW_CLOUD=1`
explicitly. coxn makes no background LLM calls; model calls are user-initiated.

At runtime, `/model` lists every model the provider advertises and `/model
<name|#>` switches mid-session on the active instance. `/model
<instance>/<name>` switches instance, and `/model @<role>` uses the `[route]`
mapping. Tab-completion and arrow-navigation are available in the picker.

### CLI piggyback backends (Codex, Claude Code, Grok Build)

Presets `codex`, `claude-cli`, and `grok-cli` run chat turns through the local
CLI (`codex app-server`, `claude -p`, `grok -p`). coxn does **not** forward its
tool schemas to the CLI — these backends are **text-only** in the harness (chrome
shows `[text-only]`). Use them for subscription-auth chat; use OpenAI-compat or
`ollama-native` for gated edits, `run_command`, and `/execute` partitions.

## Tools and the safety model

Every mutating tool call opens an **approval modal with a diff preview** before it runs:

```
Approve edit src/lib.rs?
[o] once  [s] session  [d] decline  [x] cancel
```

File edits show a unified diff; `run_command` shows the command and sandbox labels.

For `run_command`, session-approval is scoped to the **exact command string**, not
the tool name, so re-approving a different command is always required.

### Always-available tools

| Tool | What it does |
|---|---|
| `read_file` | Read a file (confined to project root) |
| `edit` | Replace an exact unique string in a file (confined to project root) |
| `write_file` | Write a whole file (confined to project root) |
| `run_command` | Run a shell command in the bwrap sandbox |

File tools (`read_file`, `edit`, `write_file`) are confined to the project root
via a canonicalize check -- a path that resolves outside the root is rejected
before any approval prompt.

### run_command sandbox

When `bwrap` is present on the host (install via your package manager):

- Runs in fresh Linux namespaces (`--unshare-all`)
- Project root is read-write; the rest of the filesystem is read-only
- **No network** unless the model requests `network: true`
- Environment is cleared -- API keys in the parent process never leak in
- Output is capped (120 head + 120 tail lines; 60 000 character hard cap)
- Wall-clock timeout (default 300 s; `COXN_RUN_TIMEOUT_SECS` overrides)
- Ctrl-C cancels the running command and kills the child

When `bwrap` is absent, the command runs directly with the same cleared
environment and cwd pinned to the project root; the approval prompt is then the
only boundary. The approval prompt labels this: `[sandbox]` vs `[NO SANDBOX]`.

### Optional aden tools (when aden is on PATH)

| Tool | What it does |
|---|---|
| `aden_asm` | Assemble token-budgeted context around an anchor set |
| `aden_understand` | Definition + callers + downstream impact for a symbol |
| `aden_grep` | Structure-aware search (hits tagged with enclosing symbol) |
| `aden_ask` | Natural-language question over the code graph |
| `aden_locate` | Symbol definition and call sites |

### The scope gate (aden task mode)

Set `COXN_TASK_NAME` + `COXN_TASK_SEEDS` and aden defines a scope manifest.
Approved file edits are then gated: coxn applies the edit, runs `aden
impact-diff --scope`, and reverts the file if it escapes the scope.

Commands cannot be snapshot-reverted; if a command's effects escape scope, the
gate detects and reports the violation rather than reverting.

```sh
COXN_TASK_NAME=refactor-cache \
  COXN_TASK_SEEDS=CacheStore \
  COXN_TASK_BUDGET=8192 \
  cargo run
```

Without a task, or without aden, edits are gated by your approval only. The
status line is honest about which mode is active.

## Environment variables

| Variable | Effect |
|---|---|
| `COXN_MODEL_BASE_URL` | OpenAI-compatible endpoint base URL |
| `COXN_MODEL_NAME` | Model name to request |
| `COXN_MODEL_KEY` | API key (never written to config) |
| `COXN_KEY_<INSTANCE>` | API key for a named provider instance, e.g. `COXN_KEY_OPENROUTER` |
| `COXN_ALLOW_CLOUD` | Allow a cloud provider instance without a key when set to `1` |
| `COXN_ADEN_BIN` | Path to aden binary (default: `aden` on PATH) |
| `COXN_TASK_NAME` | Task name; activates aden scope gate |
| `COXN_TASK_SEEDS` | Comma-separated seed symbols for the scope |
| `COXN_TASK_BUDGET` | Token budget for scope context (default 8192) |
| `COXN_RUN_TIMEOUT_SECS` | Wall-clock timeout for run_command (default 300) |
| `COXN_VIM` | Set to `1` for vim-style Normal/Visual/Command modes (default: chat-first insert-only) |
| `COXN_TUI3` | Structured shell on by default; set `0` for legacy single-pane. `Ctrl-T` collapse tools, `Ctrl-Shift-R` toggle reasoning |
| `COXN_CLIPBOARD` | Set to `on` or `1` for OSC52 transcript copy on selection |

## Slash commands and keys

```
/help            show this help
/model           list models (* = active, [loaded] = hot in memory)
/model <name|#>  switch the active model
/model @<role>   switch to the model mapped for a role (route.<role> config)
/think <level>   reasoning effort: off | low | med | high
/tools           list the aden tools the model can discover
/agents          show the task partition (sub-scopes + routed models)
/session         list saved sessions
/resume <slug>   load a saved session
/edit [path]     open the last-edited file (or path) in $EDITOR
/clear           clear the conversation (keeps the task scope)
/quit            leave coxn
/scope           show active task scope (COXN_TASK_*)
/trust           toggle read_file session-auto approval
/undo            revert last accepted file edit via git checkout
/export          save transcript to ~/.local/share/coxn/exports/
/copy            save transcript to ~/.local/share/coxn/last-transcript.txt
/auth status     check configured provider auth
/auth list       list provider presets
/auth login <id> print native login or key setup command
/execute         run aden task partition (live progress; transcript preserved)
/runs            list execution run ledgers (JSONL under ~/.local/share/coxn/runs/)
/runs <slug>     summarize a run (approvals, gate blocks, usage)
```

CLI:

```
coxn doctor              health check
coxn auth status         check configured provider auth
coxn auth list           list provider presets
coxn auth login <id>     print native login or key setup command
coxn auth set-key <id>   write ~/.config/coxn/secrets/<id>.key from stdin
coxn once -p "prompt"    headless turn (COXN_AUTO_APPROVE=1)
```

Use `@path/to/file` in messages to inject file contents (up to 3 files), or type `@`
to open a fuzzy file picker.

Prefix a line with `!` to run a shell command locally without a model turn. Coxn
shows a **y/n confirm** first (human gate, same keys as scope blocks). When `bwrap`
is on PATH the command runs sandboxed; otherwise you see a NO SANDBOX warning.
Output streams to the transcript; Ctrl-C cancels mid-run.

`/model` and `/session` open an arrow-navigable picker (Up/Down, Enter, Esc).

Keys (default chat-first; set `COXN_VIM=1` for full vim modes):

The status line shows `mode: CHAT` and a one-line tip on boot (dismiss with any
key). Press `?` with an empty input to open the help overlay; `g?` also works.

| Key | Action |
|---|---|
| Enter | Send message |
| Alt-Enter / Shift-Enter | Newline in input |
| Ctrl-C | Cancel a turn / quit when idle |
| Ctrl-Space / Ctrl-P | Fuzzy command palette |
| Ctrl-F / Ctrl-Shift-F | Transcript search (when vim off) |
| Tab | Command palette / completion |
| @ | Attach project file (fuzzy picker) |
| Up/Down | Empty input: recall prior prompts; while typing: scroll chat; in pickers: move selection |
| PgUp/PgDn | Scroll a page |
| Ctrl-N | History forward while browsing (`Ctrl-P` opens palette) |
| Ctrl-W | Delete word |
| Ctrl-K / Ctrl-U | Cut to end / cut to start |
| Ctrl-Y | Yank (paste) |
| Left/Right Home/End | Move cursor |
| ? / g? | Help overlay (chat-first: `?` when input is empty) |
| o/s/d/x | Tool approval modal (once / session / decline / cancel) |
| y/n | Gate-block proceed / cancel |
| Ctrl-L/A/I/V/G | ADEN ops on word at cursor (Insert mode) |

With `COXN_VIM=1`: Esc → Normal, `/` transcript search, `gr` aden grep, `K`/`gd` understand, `:view` / `:doctor` ex commands.

## Model routing by role

Map roles to models in `.aden/config.toml` and switch by role with
`/model @<role>`:

```sh
aden config set route.scout  qwen2.5-coder
aden config set route.synth  <capable model>
```

Roles are open-vocabulary keys; coxn looks them up (selection is data, not
inference).

## Sessions

Every message is appended to a JSONL session under
`$XDG_DATA_HOME/coxn/sessions/` (or `~/.local/share/coxn/sessions/`), so a
crash loses nothing. `/session` lists saved sessions; `/resume <slug>` continues
one.

## Resilience

- **Streaming** replies render token-by-token; Ctrl-C cancels a turn or quits
  when idle.
- **Auto-retry**: transient backend errors retry with a backoff and a cancellable
  countdown.
- **Context meter**: the status line shows `~Nk ctx` (last turn's token usage).
- **Hot models**: auto-detect prefers a model already loaded in memory; `/model`
  marks `[loaded]` ones where the server reports state.

## Dependencies

Six, deliberately: `ratatui` + `crossterm` (TUI), `tokio`
(`rt`/`macros`/`process`/`io-util`/`time`), `serde` + `serde_json` (wire
types), `ureq` (OpenAI-compatible HTTP). Minimalism is the product.

## Status

Working and validated end to end against a real codebase + a local model:
provider-neutral chat, streaming replies with live run_command output, tool
calling, model enumeration/switching, hot-model detection, per-role routing, the
read/edit workflow with per-tool approval, the bwrap sandbox, the optional aden
scope gate (with revert on escape), auto-retry, the context meter, and JSONL
session persistence (`/resume`).

Not yet: multi-model sub-agents (`DESIGN.adoc` Phase 5); a native streaming
profile for servers whose chat-completions layer drops tool calls under streaming;
an optional direct-provider profile (native caching / exact billing).
