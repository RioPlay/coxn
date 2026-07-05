# coxn production checklist

Task-only backlog to reach production-complete state. No timelines.
Mark items `[x]` as they land; log passes in `LOG.md`.

**North star:** standalone gated harness works; aden-amplified partition works;
every action explainable via ledger; wedge obvious in one minute; pump stays dumb;
distribution real; limits honest.

**Production gate:** Workstreams 1, 2, 3, 5, 6 complete + health stack green +
`PLAN.adoc`/README synced + `main` clean.

---

## Workstream 1 — Trust & proof

- [x] Record `scripts/demo-scope-escape.sh` as README visual (asciinema + agg GIF)
- [x] `scripts/record-scope-escape.sh` helper (asciinema/vhs/manual fallback)
- [x] Embed visual in README "Why coxn is different" (static transcript; animated cast optional)
- [x] Document dirty-tree + active-scope behavior in README
- [x] Document dirty-tree behavior in `/scope` help
- [x] Boot warning when scope active + dirty tree (`drive.rs`)
- [x] `coxn doctor` warns on dirty tree + active scope
- [x] Add `scripts/smoke-ollama.sh`
- [x] Add `scripts/smoke-execute-partition.sh`
- [x] Add `scripts/smoke-gate.sh` (CI-friendly wrapper)
- [x] `smoke-gate.sh` passed locally (aden present)
- [x] Run `smoke-ollama.sh` on host with ollama; record in `LOG.md`

**Done when:** README shows wedge visually; smokes scripted and pass locally;
dirty-tree failure is actionable.

---

## Workstream 2 — Full observability

### 2a — Event schema

- [x] Lock event kinds in docs (`docs/contract.adoc` run-ledger section)
- [x] Kinds documented: `run_started`, `scope_started`, `model_selected`,
      `assistant_delta`, `tool_call`, `approval`, `tool_result`, `file_edit`,
      `command_output`, `gate_verdict`, `usage`, `turn_started`, `turn_finished`,
      `scope_finished`, `run_finished`

### 2b — Wire ledger into pump (`TurnIo`)

- [x] `LedgerTurnIo` wrapper records approval, tool call/result, gate, usage, deltas
- [x] Chat drive loop wraps `DriveIo` with `LedgerTurnIo`
- [x] `/execute` sequential path uses shared `LedgerTurnIo` (replaces `LedgerBatchIo`)
- [x] Ledger write failure never breaks TUI run (append no-ops when dir missing)

### 2c — User-facing inspection

- [x] `/runs` — list recent run slugs
- [x] `/runs <slug>` — enhanced summary (models, approvals, gate blocks, usage)
- [x] Normal chat turns create/append ledger (`chat` scope per session)

### 2d — Parallel path parity

- [x] Granular per-tool events on parallel `/execute` path

**Done when:** Gated edit → approval + edit + gate_verdict in JSONL; `/runs` works
after restart; `/execute` fully reconstructable.

---

## Workstream 3 — Loop hardening

- [x] Parse grok NDJSON usage → context meter (when CLI emits `usage` on `end`)
- [x] Unit test grok usage with fake binary
- [x] Cancel through `pump.run_turn_streaming` for normal turns
- [x] Unit test Ctrl-C mid-stream preserves partial text
- [x] Throttle/diff-append `/execute` progress snapshots
- [x] Cancel in-flight parallel workers on partition cancel
- [x] Consolidate env test locking → shared `ENV_TEST_LOCK`
- [x] Idle perf: zero `aden` spawns in `drive()` idle loop
- [x] Run `devex-review`; fix or accept P1/P2 in `LOG.md`

**Done when:** Streaming cancellable; usage honest; idle dogfood clean;
devex-review clear.

---

## Workstream 4 — Architecture & laws

- [~] Split `drive.rs` → `drive/{mod,input,streaming,slash,boot}.rs` (`ex.rs`, `input.rs` landed)
- [ ] Move `tui.rs` render into `ui/render.rs`
- [ ] No module > ~1.5k LOC
- [ ] Deferred tool discovery: search → append schemas next turn
- [x] Unit test mid-session `aden_grep` discovery
- [x] Optional aden preamble nudge when aden on PATH
- [x] Sync `PLAN.adoc` (close stale items — Phase 5, O3, aden branch note)
- [x] Sync README "Not yet" section
- [x] Bump `VERSION` + `CHANGELOG` per workstream ship (0.5.2.0)

**Done when:** Small modules; deferred discovery works; docs match code.

---

## Workstream 5 — Onboarding & distribution (ship gate)

### 5a — First-run

- [x] Multi-backend hot-swap
- [x] Preset readiness probes + categorized pickers
- [x] Preset readiness badges in `coxn doctor`
- [x] `probe_preset` tests per driver (fake binaries)
- [x] `coxn doctor` answers "what can I use right now?" in one screen

### 5b — Distribution

- [x] Verify `cargo install --path .` on clean walkthrough
- [x] Verify crates.io release workflow (`cargo package` + tag GH release; publish manual with `CRATES_IO_TOKEN`)
- [x] `check-no-secrets.sh` in CI
- [x] `smoke-gate.sh` in CI (skips without aden)
- [x] `pty-smoke.sh` in CI

### 5c — Documentation lock

- [x] README matches bindings, gates, text-only CLI stance (partial — Status/Not yet synced)
- [x] INSTALL.md matches auth presets (+ `/execute`/`/runs`/jobs)
- [x] Help overlay matches README (`:execute` threat note)
- [x] Document `/execute` auto-approve threat model
- [x] Document aden caveats (OO recall, prose `ask`) in README + doctor

**Done when:** Fresh install → doctor → first gated edit without reading DESIGN.adoc.

---

## Workstream 6 — Sub-agent production

- [x] Partition consumption (`agents.rs`, `/agents`)
- [x] Sequential `/execute` with role routing
- [x] Per-role `ToolPolicy` + per-scope budgets
- [x] Adaptive stopping (hop cap, tool-error abort, `COXN_SUBAGENT_MAX_TURNS`)
- [x] `/execute --resume`
- [x] Parallel read-only scopes (`COXN_EXECUTE_JOBS`)
- [x] Partition index smoke passes (`smoke-execute-partition.sh` — hermetic, no live model)
- [ ] Live model partition smoke passes (scout/synth end-to-end)
- [x] Verify dense merge upstream (hermetic `execute_partition` sequential path + unit tests)
- [x] Confirm text-only routes refused (`execute_route_guard` unit tests; live smoke optional)
- [x] Mark Phase 5 complete in `PLAN.adoc`

**Done when:** Partition smoke passes; ledger shows per-scope models, usage, gates.

---

## Workstream 7 — Optional (post-ship)

- [ ] Linux seccomp filter (`sandbox.rs`)
- [ ] Non-Linux sandbox doc or `scripts/run-in-docker.sh`
- [x] Anthropic-direct profile — won't do (OpenRouter covers cloud; optional later per DESIGN)
- [x] CLI piggyback hybrid bridge — won't do (text-only permanent; tools stay on pump)

---

## Workstream 8 — aden cross-repo

- [x] aden 0.2.0 gate contract on PATH
- [x] Remove stale PLAN note re `feat/coxn-directional-prereqs`
- [ ] Track `feat/vocab-mismatch-evals` on aden repo
- [ ] Re-run gate demo on OO fixture when aden recall improves
- [ ] `aden view --watch` bridge — optional post-launch

---

## Already done (baseline)

- [x] Main synced to `origin/main` (preset readiness + hot-swap)
- [x] 346 tests, fmt/clippy clean
- [x] Three-gate core (human, bwrap, scope revert)
- [x] TUI 3.0 structured shell
- [x] Provider instances + CLI piggybacks
- [x] `scripts/demo-scope-escape.sh` + `scripts/demo-sandbox.sh`
- [x] Run ledger coarse events for `/execute`
- [x] `coxn once` headless mode

---

## Health stack (run every pass)

- [x] `cargo fmt --check`
- [x] `cargo clippy -- -D warnings`
- [x] `cargo test`
- [x] `bash scripts/check-no-secrets.sh`

See also `CHECKLIST.md` for per-pass trust/TUI gates.

## TUI hot path (perf)

- [x] Run ledger: no per-token/per-line JSONL sync writes during streaming
- [x] Status line: savings from `.aden/savings.json` (no `aden status` subprocess)
- [x] Ctrl-Space palette: no `probe_preset` storm, no embedded full model list
- [x] Tab commands menu: no `aden list` subprocess on every open
- [x] Backend discovery cache (20s TTL) + invalidate on switch/setup
- [x] Throttle TUI repaint during streaming (~33ms min interval)