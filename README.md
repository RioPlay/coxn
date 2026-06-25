# coxn

A lean, provider-agnostic terminal harness that drives an LLM which drives
[aden](https://github.com/) for context and gating.

`coxn` is a *dumb pump*: it steers and sets pace and carries no intelligence.
The split is exact:

- **aden** directs and gates: it owns the graph, the task scope, the
  blast-radius gate, and the savings estimate. Deterministic, offline.
- **the LLM** acts: it reads, reasons, and writes (edits).
- **coxn** steers: it runs the loop, dispatches tools, enforces aden's gate, and
  renders the TUI. Nothing more.

The name is coxswain shorthand (`cox'n`): the cox steers the boat and calls the
cadence but never rows. See `DESIGN.adoc` for the full design and laws.

## Quickstart

```sh
# With a local model server running (Ollama or LM Studio), coxn auto-detects it:
cargo run
```

coxn probes Ollama (`:11434`) then LM Studio (`:1234`) and uses the first live
one. With nothing running it falls back to an offline stub that just echoes, so
the TUI still comes up.

Type to chat; the model's replies render in the pane above the input line.
`Ctrl-C` quits.

## Choosing a model

Selection is data, resolved in this order:

1. `COXN_MODEL_BASE_URL` (+ `COXN_MODEL_NAME`, `COXN_MODEL_KEY`) in the env
2. `.aden/config.toml` (`model.base_url`, `model.name`) via `aden config set`
3. local auto-detection (Ollama, then LM Studio)
4. the offline stub

Any OpenAI-compatible endpoint works (LM Studio, Ollama, vLLM, OpenRouter,
OpenAI, ...). Secrets come from `COXN_MODEL_KEY` in the env, never the committed
config.

```sh
COXN_MODEL_BASE_URL=http://localhost:1234/v1 COXN_MODEL_NAME=openai/gpt-oss-20b cargo run
```

At runtime, `/model` lists every model the provider advertises (loaded or not)
and `/model <name|#>` switches the active one mid-session.

## Editing: approval, and an optional scope gate

The model can edit via the `edit` (replace an exact unique string) and
`write_file` tools, plus `read_file` to fetch the exact text first. Every
mutating call is **approved at the prompt** before it runs:

```
approve edit src/lib.rs?  [o]nce  [s]ession (all edit calls)  [d]ecline  [x] cancel turn
```

`[s]ession` stops prompting for that tool until `/clear`. Edits are confined to
the project root.

A **task** adds aden's blast-radius gate on top. Set a task name and seed
symbols and aden defines the scope:

```sh
COXN_TASK_NAME=tune-worker COXN_TASK_SEEDS=makeDrainableWorker cargo run
```

- `aden scope` resolves the seeds into a file mandate + token-budgeted context,
  loaded into the prompt (and nothing more), and a one-line operating preamble
  is added so the model knows to act via the tools.
- An approved edit is then gated: coxn applies it, runs `aden impact-diff
  --scope`, and if it escapes the scope it **reverts that one file** and feeds
  the verdict back to the model. In-scope edits persist.

Env vars: `COXN_TASK_NAME`, `COXN_TASK_SEEDS` (comma-separated),
`COXN_TASK_BUDGET` (tokens, default 8192).

## Resilience and feedback

- **Streaming** replies render token-by-token; `Ctrl-C` cancels a turn (or quits
  when idle).
- **Auto-retry**: transient backend errors (rate limit, unavailable, dropped
  connection) retry with a backoff and a cancellable countdown.
- **Context meter**: the status line shows `~Nk ctx` (last turn's token usage),
  so you can `/clear` before it grows unwieldy.
- **Hot models**: auto-detect prefers a model already loaded in memory, and
  `/model` marks `[loaded]` ones (where the server reports state).

## Sessions

Every message is appended to a JSONL session under
`$XDG_DATA_HOME/coxn/sessions/` (or `~/.local/share/...`), so a crash loses
nothing. `/session` lists saved sessions; `/resume <slug>` continues one.

## Slash commands

```
/help            show help
/model           list models (* = active, [loaded] = hot in memory)
/model <name|#>  switch the active model
/model @<role>   switch to the model mapped for a role (see Roles below)
/think <level>   reasoning effort: off | low | med | high
/tools           list the aden tools the model can discover
/session         list saved sessions
/resume <slug>   load a saved session
/edit [path]     open the last-edited file (or path) in $EDITOR
/clear           clear the conversation (keeps the task scope)
/quit            leave coxn
```

Keys: Up/Down scroll the transcript (PgUp/PgDn by a page); Ctrl-P/Ctrl-N recall
input history; Ctrl-W word-delete; Ctrl-K/Ctrl-U cut, Ctrl-Y yank; arrows/Home/
End move the cursor.

## Roles (model routing)

Map roles to models in `.aden/config.toml` and switch by role with
`/model @<role>`:

```sh
aden config set route.scout       qwen2.5-coder
aden config set route.synth       <a capable model>
aden config set route.orchestrate <the strongest model>
```

Roles are open-vocabulary keys; coxn just looks them up (selection is data, not
inference). A common split is a cheap/local model for scouting and a stronger one
for synthesis. This is the seam multi-model sub-agents will use to pick a model
per scope (see `docs/routing.adoc`).

## aden

coxn drives the `aden` binary as a subprocess (no linked crate, to keep the
dependency budget small). It must be on `PATH`, or point at a build with
`COXN_ADEN_BIN`:

```sh
COXN_ADEN_BIN=/path/to/aden/target/release/aden cargo run
```

## Dependencies

Deliberately few: `ratatui` + `crossterm` (TUI), `tokio` (current-thread
runtime), `serde` + `serde_json` (wire types), `ureq` (the OpenAI-compatible
HTTP call). Minimalism is the product.

## Status

Working and validated end to end against a real codebase + a local model:
provider-neutral chat, streaming replies, tool-calling (incl. under streaming),
model enumeration/switching + hot-model detection + per-role routing, the read
-> edit workflow with per-edit approval and the optional aden scope gate,
auto-retry, the context meter, and JSONL session persistence (`/resume`).

Not yet: multi-model **sub-agents** (`DESIGN.adoc` Phase 5; the aden partition
contract is in `docs/routing.adoc`); a native streaming profile for a local
server whose chat-completions layer drops tool calls under streaming; an
optional direct-provider profile (native caching / exact billing).
