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

## Task mode: scoped, gated editing

A *task* is what turns coxn from a grounded chat into a harness. Set a task name
and seed symbols, and aden defines the scope:

```sh
COXN_TASK_NAME=tune-worker COXN_TASK_SEEDS=makeDrainableWorker cargo run
```

- `aden scope` resolves the seeds into a file mandate and a token-budgeted
  context, which coxn loads into the prompt (and nothing more).
- The **action tools** `edit` and `write_file` are advertised only in task mode.
- Every edit is gated: coxn applies it, runs `aden impact-diff --scope`, and if
  the change escapes the scope (scope-escape / blast-leak) it **reverts that one
  file** and feeds the verdict back to the model. In-scope edits persist.

Without a task there is no gate, so editing is off (no ungated edits).

Env vars: `COXN_TASK_NAME`, `COXN_TASK_SEEDS` (comma-separated),
`COXN_TASK_BUDGET` (tokens, default 8192).

## Slash commands

```
/help            show help
/model           list available models (* = active)
/model <name|#>  switch the active model
/model @<role>   switch to the model mapped for a role (see Roles below)
/tools           list the aden tools the model can discover
/clear           clear the conversation (keeps the task scope)
/quit            leave coxn
```

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
provider-neutral chat, **streaming** replies, tool-calling over the wire (incl.
under streaming), model enumeration/switching, and scoped, gated editing
(apply -> `impact-diff --scope` -> revert on escape).

Not yet: Ollama's native `/api/chat` profile (its OpenAI-compat layer drops tool
calls under streaming; the OpenAI-compat path here covers LM Studio, OpenAI,
OpenRouter, vLLM); an Anthropic-direct profile; multi-model sub-agents
(`DESIGN.adoc` Phase 5).
