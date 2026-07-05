# Installing coxn

## Prerequisites

- A model server **or** cloud API key:
  - Local: [Ollama](https://ollama.com) or LM Studio on `:11434` / `:1234`
  - Cloud: configure a provider profile and set a per-instance key
- Optional: `bwrap` (bubblewrap) for filesystem/network sandbox
- Optional: [aden](https://github.com/RioPlay/aden) ≥ 0.2.0 for blast-radius scope gate

## From source (developers)

```sh
git clone https://github.com/RioPlay/coxn.git
cd coxn
cargo install --path .
```

Requires Rust ≥ 1.85 (edition 2024).

## Health check

```sh
coxn doctor
coxn auth status
```

Exit `0` = ready, `1` = blocking (no model), `2` = warnings (no bwrap, etc.).

## Provider profiles

For local-first use, coxn auto-detects logged-in CLIs (grok/claude/codex), native
Ollama, then LM Studio. For external providers, use the setup wizard or hand-edit
`.aden/config.toml`:

```sh
coxn                           # TUI — Ctrl-Space → setup grok-cli / ollama-native / openrouter-claude
coxn auth setup                # list presets (CLI piggybacks, OpenRouter, OpenAI, local)
coxn auth setup openrouter-claude
coxn auth set-key openrouter   # paste key in TUI, or: coxn auth set-key openrouter < key.txt
coxn auth status
coxn doctor
```

CLI piggyback presets (`codex`, `claude-cli`, `grok-cli`) are **text-only** in coxn
(chat turns). Route `scout`/`synth`/`orchestrate` to `ollama-native` or OpenRouter
for `/execute` partitions that need gated edits and `run_command`.

Or add profiles manually:

```toml
[provider.openrouter]
driver = "openai_compat"
base_url = "https://openrouter.ai/api/v1"
enabled = true

[route]
active = "openrouter:anthropic/claude-sonnet-4"
```

Keep secrets out of the repo:

```sh
export COXN_KEY_OPENROUTER=sk-or-...
export COXN_ALLOW_CLOUD=1
```

`COXN_ALLOW_CLOUD=1` is required only when no key is resolved. A key can also
live in `~/.config/coxn/secrets/openrouter.key`:

```sh
coxn auth set-key openrouter < key.txt
```

## Quick start

```sh
cd your-project
coxn                  # interactive TUI
coxn once -p "..."    # headless (requires COXN_AUTO_APPROVE=1)
```

## Scoped task mode (aden)

```sh
COXN_TASK_NAME=fix-parser COXN_TASK_SEEDS=parse_config coxn
```

Inside the TUI: `/scope` shows the active task, `/execute` runs the aden partition
(you confirm the preview first; sub-agents auto-approve tools inside each scope).
`/runs` lists JSONL run ledgers; `/runs <slug>` summarizes approvals and gates.

Parallel read-only scopes: `COXN_EXECUTE_JOBS=2` (max 8). Mutating scopes always
serialize so the blast-radius gate stays correct.

## Environment variables

See `coxn --help` or README.md.
