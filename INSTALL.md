# Installing coxn

## Prerequisites

- A model server **or** cloud API key:
  - Local: [Ollama](https://ollama.com) or LM Studio on `:11434` / `:1234`
  - Cloud: set `COXN_MODEL_BASE_URL` + `COXN_MODEL_KEY`
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
```

Exit `0` = ready, `1` = blocking (no model), `2` = warnings (no bwrap, etc.).

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

Inside the TUI: `/scope` shows the active task, `/execute` runs the aden partition.

## Environment variables

See `coxn --help` or README.md.