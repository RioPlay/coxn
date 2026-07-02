# Changelog

All notable changes to this project are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed

- **Phase K architecture extract**: `run_tui`, `drive`, and TUI helpers moved
  from `main.rs` to `src/drive.rs`; `main.rs` is CLI routing only (87 LOC).
  `build_registry` and `register_aden_tools` moved to `tools.rs`. No behaviour
  change; 298 tests green.

## [0.3.1.0] - 2026-07-02

### Added (provider setup)

- **`coxn auth setup`**: lists built-in provider presets and writes
  `.aden/config.toml` for local Ollama/LM Studio, OpenAI direct, and
  Claude/GPT/Gemini/Grok via OpenRouter (`/auth setup <id>` in the TUI).
  Keys still live in `COXN_KEY_<INSTANCE>` or `coxn auth set-key`.

### Added (TUI frictionless — M1–M3)

- **Multiline input**: Alt-Enter and Shift-Enter insert newlines; Enter always
  submits; bracketed paste for multi-line prompts.
- **Transcript search**: vim-style `/` and `?` in Normal mode; aden symbol-grep
  moved to `gr`; help overlay moved to `g?`.
- **Diff hunk rendering**: green/red/cyan diff lines in the approval modal and
  transcript via `paint_diff_line`.

### Added (TUI frictionless — M6)

- **Mode cheat-sheet + status chips**: `g?` flashes a dim one-line tip per vim
  mode (`INSERT`/`NORMAL`/`VISUAL`/`COMMAND`); auto-hides after 5s idle.
  Status string uses labeled chips (`model:`, `scope:`, `ctx:`) and the mode
  tag reads `mode: INSERT` for at-a-glance parsing.

### Added (TUI frictionless — M5)

- **Mouse + OSC52 clipboard**: picker row clicks select; gate-modal hint zone
  answers proceed/block; click-to-place-cursor in the input box; wheel scroll;
  drag-select in the transcript copies via OSC52 when `COXN_CLIPBOARD=on` or
  `1` (emitted after frame flush). Mouse is ignored while the help overlay is
  open.

### Added (TUI frictionless — M4)

- **Fuzzy command palette** (`Ctrl-Space`): subsequence-filtered picker unifying
  slash verbs, advertised models, saved sessions, and the last eight submitted
  commands. Type-to-filter with `j`/`k` navigation; `/model` and `/session`
  direct pickers unchanged. Tab still opens the ADEN-oriented commands menu.

### Added (next-generation harness)

- **Parallel `/execute`** (opt-in): `COXN_EXECUTE_JOBS` (default `1`, clamped
  `1..=8`) runs independent read-only sub-scope pumps concurrently on worker
  threads. Correctness invariant -- `aden impact-diff` judges the whole
  working tree -- so only read-only scopes run in parallel (their pump never
  invokes the gate); mutating scopes always serialize on the driving thread.
  `jobs == 1` and `--resume` take the unchanged sequential path verbatim.
- **Adaptive stopping**: a configurable per-sub-agent turn cap
  (`Pump::set_max_turns`, `COXN_SUBAGENT_MAX_TURNS`) bounds a stalling scope
  tighter than the global hop cap; the repeated-tool-error abort is already in
  place.
- **Native Ollama `/api/chat` backend**: `ProviderDriver::Ollama` +
  `AnyModel::Ollama` (`src/ollama.rs`) -- NDJSON streaming, function tools, usage
  from `prompt_eval_count`/`eval_count`, and tool-call dedup. `ollama_model`
  constructor; `/auth` and `/doctor` reachability probes. Fixes streaming-plus-
  tools for local users (Ollama's OpenAI-compat layer drops tool-call deltas).
- **Scope-escape demo**: `scripts/demo-scope-escape.sh` -- a deterministic,
  no-cloud-keys script that builds a throwaway repo and proves an in-scope edit
  is allowed, an out-of-scope edit is blocked (gate exit 1) and reverted to
  HEAD, and `coxn doctor` labels the sandbox state. README gained a "Why coxn
  is different" three-gate wedge section.
- `src/app.rs`: the model-selection + session-wiring core (`Endpoint`,
  `ModelSel`, `openai_model`/`ollama_model`, `resolve_instance_from_config`,
  `resolve_role`, `task_config`, `AGENT_PREAMBLE_*`) extracted out of
  `main.rs` (2915 -> 2755 lines); `main.rs` is now CLI routing + the TUI drive
  loop.
- `run_ledger.rs`: `COXN_RUNS_DIR` override (also a test hook) so ledger tests
  no longer contend on `XDG_DATA_HOME` with other modules.
- `aden::files_from_manifest` for the parallel scheduler's disjoint-mandate
  diagnostic.

### Changed / Fixed (audit hygiene)

- Clarified "ungated" vs gated semantics in Pump docs, boot/status line, approval prompts, and command output labels ("ungated (human approval only)", "NO SANDBOX (human approval is the only gate)").
- Replaced `.expect` panics in sandbox lib paths (argv, stdout pipe) with proper error `RunOutcome`s.
- Corrected stale Pump struct documentation describing gate=None behavior (matches reality and tests: approval is the gate; effects stand when no scope).
- Updated PLAN.adoc P4.3 description for accuracy (superseded semantics).

## [0.2.0] - 2026-06-25

### Added

- `run_command` tool: the model can run shell commands and close the
  edit-build-test loop. Requires explicit approval before every run.
- bwrap sandbox for `run_command`: when `bwrap` is present, commands run in
  fresh Linux namespaces (project root read-write, rest of filesystem
  read-only, network off by default, environment cleared so API keys never
  leak in). Falls back to direct exec with cleared env when bwrap is absent.
  The approval prompt labels which confinement level is active.
- Live streaming output: `run_command` streams stdout+stderr to the TUI
  line-by-line as the command runs. Ctrl-C kills the child immediately
  instead of freezing the TUI.
- Env-cleared fallback for direct exec: same whitelisted environment as the
  bwrap path, so secrets cannot leak regardless of sandbox availability.
- `COXN_RUN_TIMEOUT_SECS` env var to override the default 300 s wall-clock
  timeout for run_command.
- Session-scoped run_command approval keyed on the exact command string
  (not just the tool name), so approving `cargo test` does not pre-approve
  a different command.
- File tool confinement: `read_file`, `edit`, and `write_file` now reject
  paths that canonicalize outside the project root before showing any
  approval prompt.
- aden-optional/amplified architecture: coxn now runs as a complete standalone
  harness (human approval gate + bwrap sandbox) with zero aden dependency.
  When aden is on PATH, five additional tools activate (`aden_asm`,
  `aden_understand`, `aden_grep`, `aden_ask`, `aden_locate`) and the
  blast-radius revert gate becomes active for file edits.
- Status line is now honest about the active mode: `scope: <task>` when the
  aden gate is active, `ungated` when running standalone or without a task.
- Live activity spinner and command-output theming in the TUI.
- `/agents` command: shows the task partition (sub-scopes and routed models)
  when a task scope is active.
- Tab completion and arrow-navigable pickers for `/model` and `/session`.
- `/think` command: set the model's reasoning effort (off/low/med/high).
- Emacs-style kill ring in the input editor (Ctrl-K, Ctrl-U, Ctrl-Y).
- `/edit [path]` command: open the last-edited file (or a given path) in
  `$EDITOR`.
- Session path traversal guard: `/resume` validates the slug to reject paths
  that escape the session directory.

### Changed

- Architecture framing shifted from "aden-required" to "aden-optional,
  aden-amplified". coxn is a complete standalone harness; aden is an
  amplifier plugged in over a subprocess seam.
- The blast-radius gate now degrades for commands: it detects and reports
  scope impact rather than reverting (commands are not snapshot-revertible).

### Fixed

- File tools reject paths that resolve outside the project root through a
  symlink (including a dangling symlink component), closing a path-escape the
  earlier component-only check missed.
- Session-scoped approvals reset when switching sessions via `/resume` or the
  session picker, so an approval granted in one session does not carry over.
- The `aden_tools` discovery seam and `/tools` list the active aden tools
  (previously they searched only the latent set, which is empty once aden is
  present).

[Unreleased]: https://github.com/RioPlay/coxn/compare/v0.3.1.0...HEAD
[0.3.1.0]: https://github.com/RioPlay/coxn/compare/v0.2.0...v0.3.1.0
[0.2.0]: https://github.com/RioPlay/coxn/releases/tag/v0.2.0
