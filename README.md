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
2. `aden config` values (`model.base_url`, `model.name`) -- only when aden is present
3. local auto-detect (Ollama `:11434`, then LM Studio `:1234`)
4. offline stub

Any OpenAI-compatible endpoint works: LM Studio, Ollama, vLLM, OpenRouter,
OpenAI. Secrets come from `COXN_MODEL_KEY`, never a committed config file.

At runtime, `/model` lists every model the provider advertises and `/model
<name|#>` switches mid-session. Tab-completion and arrow-navigation are available
in the picker.

## Tools and the safety model

Every tool call requires explicit approval before it runs:

```
approve edit src/lib.rs?  [o]nce  [s]ession (all edit calls)  [d]ecline  [x] cancel turn
```

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
| `COXN_ADEN_BIN` | Path to aden binary (default: `aden` on PATH) |
| `COXN_TASK_NAME` | Task name; activates aden scope gate |
| `COXN_TASK_SEEDS` | Comma-separated seed symbols for the scope |
| `COXN_TASK_BUDGET` | Token budget for scope context (default 8192) |
| `COXN_RUN_TIMEOUT_SECS` | Wall-clock timeout for run_command (default 300) |

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
/copy            save transcript to ~/.local/share/coxn/last-transcript.txt
/execute         run aden task partition sequentially
```

CLI:

```
coxn doctor              health check
coxn once -p "prompt"    headless turn (COXN_AUTO_APPROVE=1)
```

Use `@path/to/file` in messages to inject file contents (up to 3 files).
```

`/model` and `/session` open an arrow-navigable picker (Up/Down, Enter, Esc).

Keys:

| Key | Action |
|---|---|
| Enter | Send message |
| Ctrl-C | Cancel a turn / quit when idle |
| Tab | Complete a command or `/resume` slug |
| Up/Down | Scroll chat; also navigate pickers |
| PgUp/PgDn | Scroll a page |
| Ctrl-P / Ctrl-N | Input history |
| Ctrl-W | Delete word |
| Ctrl-K / Ctrl-U | Cut to end / cut to start |
| Ctrl-Y | Yank (paste) |
| Left/Right Home/End | Move cursor |
| K / gd | (Normal) aden understand symbol at cursor |
| ga | (Normal) aden asm context for symbol at cursor |
| gi | (Normal) aden impact for symbol at cursor |
| gv | (Normal) launch aden view for symbol at cursor |
| / | (Normal) aden grep on symbol at cursor |

: ex commands in Normal/Command mode include `:view`, `:gm`/`:viz` (mermaid), `:doctor`, `:impact`, `:understand`, `:grep`, `:ask`.

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
