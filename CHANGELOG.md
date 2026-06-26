# Changelog

All notable changes to this project are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

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

[Unreleased]: https://github.com/RioPlay/coxn/compare/HEAD...HEAD
