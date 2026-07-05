# Gated development loop — session log

## Loop definition

1. **Ship** focused change
2. **gstack review** (critical pass + adversarial)
3. **Fix** P1/P2 findings before continuing
4. **Re-review** until clean or explicitly accepted risk
5. **Record** what worked / failed in this directory
6. **Evolve** checklist when a better gate is discovered

## Pass 1 — 2026-07-02 (TUI polish + live execute)

**Commits reviewed:** `c5eb4be` … `bceecbb`

**Findings (gstack):**
- P1: `!cmd` bypassed human approval gate
- P1: `/execute` live progress wiped transcript
- P2: event loop frozen during `!cmd` / `/execute`
- P2: `COXN_VIM` test env races
- P2: mid-run execute early return dropped streamed report

**Fixes applied (pass 2):**
- `!cmd`: y/n confirm modal + `run_streaming` + Ctrl-C kill
- `/execute`: preserve `prior_output` in progress formatter
- `/execute`: `ExecuteProgress::with_cancel` between scopes
- Tests: `ENV_TEST_LOCK` mutex for `COXN_VIM` mutations
- Docs: README/help/welcome disclose gate + sandbox

**Status:** pass 2 accepted (`207b7ff`) — P1 closed, residual P2 documented

### Pass 2 — 2026-07-02 (review fixes)

**Commit:** `207b7ff`

**gstack re-review verdict:** Accept — ungated `!cmd` and transcript wipe fixed; `/execute` cancel between scopes; env test locks.

**Residual (accepted / backlog):**
- Cancel during single-scope `run_turn_streaming` (needs TurnIo hook)
- Parallel wave in-flight workers not interrupted on cancel

### Pass 3 — 2026-07-02 (TUI 3.0 wiring)

**Scope:** PR2–4 — structured layout + drive.rs routing

**Shipped:**
- `COXN_TUI3=1` opt-in: chrome / conversation / activity regions
- `drive.rs`: boot init, chrome refresh, `sync_turns`, live turn streaming
- Activity routing: `/execute`, `!cmd`, slash listings (conversation preserved)
- Dual-write `view.output` retained for `/copy` migration

**Tests:** 320 pass; clippy clean

**Next:** dogfood gates, PR5 remove dual-write, PR6 polish

### Pass 4 — 2026-07-02 (TUI 3.0 PR5–6 complete)

**Shipped:**
- Removed ui3 dual-write: conversation/activity own channels; `/copy` via `export_text()`
- Conversation + activity scroll (mouse wheel targets pane under cursor)
- Tool collapse (`Ctrl-T`), reasoning hide (`Ctrl-Shift-R`)
- `strip_reasoning` for `<think>` blocks in assistant cards

**Tests:** 323 pass; clippy clean

**Residual:** default `COXN_TUI3=1` after dogfood; aden vim paths still write legacy output when ui3 off only

### Pass 5 — 2026-07-02 (default-on + feed routing)

**Shipped:**
- `COXN_TUI3` default on (`0` disables legacy pane)
- All aden/vim/ex/sys feeds route through `push_feed` / `append_aden` when ui3
- Help overlay documents structured shell keys

**Tests:** 325 pass

**Commit:** `6e9a4f7`

### Pass 6 — 2026-07-02 (repo hygiene)

**Actions:**
- Health stack green (325 tests, clippy, fmt, secrets)
- VERSION → 0.5.0.0 + CHANGELOG for TUI 3.0
- Pruned merged stale branches (local + origin)
- Pushed `main` to origin
- Other env vars in tests without locks (lower priority)

**Quick fix after re-review:** `partition cancelled` appends instead of replacing transcript

### Pass 7 — 2026-07-03 (CLI piggyback + streaming idle + history)

**Shipped:**
- GrokCliPiggybackModel + ClaudeCliPiggybackModel (text-only via local `grok -p` / `claude -p` + streaming-json NDJSON; no tools on CLI side)
- Shared `cli_ndjson` + `stream_idle` for polling TUI `on_idle` (drain edits, history, scroll, Ctrl-C) while CLI child blocks on next line
- `CancelTrack` + `on_idle` / `stream_cancelled` on TurnIo; pump uses it for cancel during `run_turn_streaming`
- `drain_input_edits` in drive: typing/scroll/history work during any model turn (including piggybacks)
- Up/Down history recall when input empty (prevention gates on actions)
- Auth/doctor: probe_logged_in for grok + newly for claude; consistent ✓ authenticated vs blocking "not logged in"
- Rebuild fns + /model listing + resolve for the two new drivers; wired through provider enum
- Fake-binary unit tests for both new models; 335 tests green

**Also in batch:**
- Small symmetry: claude now has probe_logged_in matching grok
- Health: fmt/clippy/test/secrets all green on the changes

**Status:** feature complete for text-only CLI piggyback expansion; usage remains optional/None (no parser yet)

**Next candidates (from IMPROVEMENTS + PLAN):**
- Full cancel during single-scope streaming (TurnIo hook already present; drive integration)
- Usage extraction from grok/claude NDJSON end/result events if emitted
- Model-driven `/execute` live validation (scout/synth roles end-to-end)
- Live Ollama smoke if binary available
- Dedup `flatten_request` copy between claude/grok (low pri)

### Pass 8 — 2026-07-03 (cancel hygiene + claude usage + dedup)

**Shipped:**
- `cli_ndjson`: kill child on turn end; empty output OK when `stream_cancelled()` (Ctrl-C before first token)
- `usage_from_object` + Claude `result`/`assistant` usage → context meter
- Shared `flatten_request` in `cli_ndjson` (removed duplicate from grok/claude)
- `DriveIo.on_idle`: sets `cancelled` on Ctrl-C so `stream_cancelled` is reliable
- Tests: usage parse, cancelled-empty NDJSON turn

**Tests:** 337 pass; clippy/fmt/secrets green

**Next:** model-driven `/execute` scout/synth validation; grok usage if CLI adds it to `end` events

### Pass 9 — 2026-07-03 (onboarding bundle)

**Shipped:**
- `discover.rs`: CLI auto-detect (grok→claude→codex), native Ollama before HTTP, `cli_instance_ready` gate
- Hot-reload after `/auth setup` + palette setup presets (no `[r]` required)
- Ctrl-Space top entries: grok-cli / ollama-native / openrouter-claude; Tab menu adds `/auth`
- Chrome `[text-only]` tag for CLI piggybacks; in-TUI `/auth set-key` modal
- `provider::write_secret`; offline stub hints Ctrl-Space

**Tests:** 338 pass

### Pass 10 — 2026-07-03 (role routing + help)

**Shipped:**
- `resolve_role`: reads `[route]` from config without requiring aden on PATH
- Help overlay: `/auth setup`, Ctrl-Space presets, Ctrl-C cancel, `[text-only]` chrome note
- README resolution order + first-run path
- Test: `resolve_role_reads_config_routes_without_aden`

**Also:** `execute_partition_resolves_distinct_role_routes_without_aden` test

**Commit:** `bc3d5c1` + doctor auto-detect fix on `feat/cli-piggybacks-grok-claude`

### Pass 11 — 2026-07-03 (doctor parity + wedge)

**Shipped:** `coxn doctor` uses `discover::detect_cli` / `detect_ollama_native` — no false OFFLINE STUB when grok/claude logged in

**Validated:** `scripts/demo-scope-escape.sh` green with grok auto-detect CLI

### Loop status — 2026-07-03

**Branch objectives (N6/N10b/N11 + onboarding): COMPLETE**
- Grok/Claude/Codex CLI piggybacks, shared NDJSON seam, streaming idle, cancel hygiene
- Auto-detect + hot-reload + palette onboarding + in-TUI set-key
- Role routing without aden; hermetic scout/synth tests

**Remaining PLAN items (out of scope for this branch or blocked):**
- Live `/execute` partition smoke (needs aden + live model on host)
- Live Ollama smoke (no `ollama` binary here)
- Optional Anthropic-direct API profile (deferred in PLAN)
- README scope-escape GIF (docs asset)
- Ship: merge `feat/cli-piggybacks-grok-claude` → main via PR

### Pass 12 — 2026-07-03 (loose ends)

**Shipped:**
- CHANGELOG/VERSION 0.5.1.0; config example + README/INSTALL/PLAN sync
- `/execute` `execute_route_guard` blocks text-only active model or role routes
- `/agents` marks `[text-only]` routes; footer updated
- `discover::selection_is_text_only` + tests

**Tests:** 342 pass

### Pass 13 — 2026-07-03 (ship prep)

**Shipped:**
- `execute_route_guard_blocks_text_only_role_route` test (scout → grok-cli)
- Health stack green; `demo-scope-escape.sh` wedge green with grok auto-detect
- Branch pushed; PR #3 opened for `feat/cli-piggybacks-grok-claude` → main

**Loop closed:** N6/N10b/N11 + onboarding + loose ends complete. Live partition/Ollama smoke remain host-dependent open items in PLAN.

### Pass 14 — 2026-07-05 (production plan + WS1 start)

**Shipped:**
- `scratch/gated-loop/PRODUCTION-CHECKLIST.md` — full task backlog to production gate
- Smoke scripts: `smoke-gate.sh`, `smoke-ollama.sh`, `smoke-execute-partition.sh`
- `record-scope-escape.sh` (asciinema/vhs/manual)
- README dirty-tree caveat + record instructions
- `/scope` dirty-tree warning; `coxn doctor` setup preset readiness badges
- CI: `smoke-gate.sh` after `cargo test`

**Validated:** `smoke-gate.sh` pass (aden + demo-scope-escape wedge)

**Next:** embed scope-escape visual in README; WS2 run ledger pump wiring

### Pass 15 — 2026-07-05 (WS2 observability)

**Shipped:**
- `LedgerTurnIo` in `run_ledger.rs` — shared pump-boundary recorder
- Chat session ledger: `run_started` at boot, per-turn `model_selected` +
  `turn_started`/`turn_finished`, quit → `run_finished`
- Enhanced `/runs <slug>` summary (models, approvals, gate blocks, usage)
- Run ledger event schema in `docs/contract.adoc`
- README `/runs` docs; auth list/setup test hardened against flaky daemon probes

**Tests:** 347 pass

**Next:** WS1 visual embed; WS2d parallel ledger; WS3 grok usage + stream cancel

### Pass 16 — 2026-07-05 (TUI perf emergency)

**Root cause:** WS2 `LedgerTurnIo` synced JSONL on every stream delta; palette/menus
spawned multiple provider probes + `aden list` per open; `status_line` spawned
`aden status` after turns.

**Fixes:**
- Ledger: batch turn stats in `turn_finished`; `BufWriter` + flush per turn
- Savings: `savings_detail_from_file` (no subprocess on hot path)
- Palette: drop live preset probes + full model_menu embed
- Tab menu: drop `aden list_symbols` on every open
- `list_ready_backends` 20s cache + invalidate on switch/setup

**Tests:** 347 pass

### Pass 17 — 2026-07-05 (WS3 hardening + WS5 distribution)

**Shipped:**
- DriveIo stream repaint throttle (~33ms) + flush at turn end
- ExecuteProgress emit throttle (100ms) + flush on completion/abort
- Parallel `/execute`: shared cancel flag propagates to worker threads via `CancelBatchIo`
- `src/test_env.rs` — consolidated `ENV_TEST_LOCK` for env-mutating tests
- `probe_preset` hermetic tests (grok/claude fake binaries)
- `coxn doctor` + README aden caveats (OO recall, prose `ask`)
- CI: `pty-smoke.sh` after `smoke-gate.sh`

**Tests:** 350 pass; smoke-gate green; pty-smoke skip (headless capture)

**Next:** grok usage → context meter (blocked on CLI); WS1 visual embed; WS2d parallel ledger

### Pass 18 — 2026-07-05 (WS2d + WS5 docs)

**Shipped:**
- Parallel `/execute`: `SharedLedgerTurnIo` — per-tool ledger events without
  holding mutex across await (safe for worker threads)
- `scope_started`/`model_selected` emitted before each scope runs (parity with sequential)
- README `/execute` auto-approve threat model; help overlay `:execute` note
- Idle loop documented: chrome uses `ScopeCaches` file reads only

**Tests:** 350 pass; smoke-gate green

**Next:** WS1 visual embed (needs asciinema/vhs); grok usage; INSTALL/help doc lock

### Pass 19 — 2026-07-05 (WS3 cancel + grok usage + docs)

**Shipped:**
- `Model::stream` default respects `on_idle` cancel before emitting full text
- `cancel_mid_stream_preserves_partial_text` pump test (ChunkedStreamModel)
- Grok CLI: parse `usage` from `end` / `usage` NDJSON events + fake-binary test
- README Status/Not yet synced with Phase 5 ship state
- INSTALL: `/execute` threat boundary, `/runs`, `COXN_EXECUTE_JOBS`

**Tests:** 352 pass; `cargo install --path .` OK

**Next:** push main; WS1 visual; live smokes; devex-review

### Pass 20 — 2026-07-05 (WS5 doctor + devex + PLAN)

**Shipped:**
- `coxn doctor` compact one-screen default (~17 lines): `ready now` preset
  summary, grouped `setup` hints, config issues demoted to warnings when not
  routed; `COXN_DOCTOR_VERBOSE=1` restores full preset table
- Non-active provider misconfig (e.g. openrouter without key) warns instead of
  blocking when auto-detect model works
- `discover::summarize_presets()` for doctor + tests
- PLAN.adoc: Phase 5 marked done; O3 parallel ledger note synced; stale aden
  branch note closed

**Devex-review (CLI audit, no browser):**
- P1: none
- P2 fixed: doctor falsely NOT READY when unused cloud instance lacked key
- P2 accepted: WS1 visual embed blocked (no asciinema/vhs); live partition/Ollama
  smokes host-dependent
- TTHW: `cargo install --path .` ~4s warm; doctor answers "ready now" in one screen

**Validated:** 353 tests; smoke-gate green; `cargo package` OK

**Next:** WS1 visual (needs recorder); live smokes; WS4 module split (deferred)

### Pass 21 — 2026-07-05 (WS6 partition smoke)

**Shipped:**
- `smoke-execute-partition.sh` uses coxn repo aden store + seed `run`; runs
  `agents` + `run_ledger` unit tests; cleans `.aden/agents/coxn-smoke-*` artifacts
- CI: `smoke-execute-partition.sh` after `smoke-gate.sh`

**Validated:** smoke pass locally (aden + indexed store)

### Pass 22 — 2026-07-05 (WS1 visual embed)

**Shipped:** README wedge section — static demo transcript under
`demo-scope-escape.sh` (interim until asciinema/vhs on host)

### Pass 23 — 2026-07-05 (WS4 start + 0.5.2.0)

**Shipped:**
- `drive/ex.rs` — vim `:command` parser extracted from drive loop
- Aden preamble nudge when aden on PATH (ungated sessions)
- `mid_session_aden_registration_enables_grep_discovery` unit test
- VERSION/CHANGELOG 0.5.2.0 (passes 19–22 rollup)

**Host smokes:**
- `smoke-ollama.sh` skip: ollama not reachable at :11434 (accepted)

**Validated:** 355 tests; fmt/clippy/secrets/smoke-gate green

### Pass 24 — 2026-07-05 (WS4 input split)

**Shipped:** `drive/input.rs` — streaming idle input drain + `pane_dims` helper

**Validated:** 355 tests green