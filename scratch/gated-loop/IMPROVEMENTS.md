# Loop improvements discovered in review

| Date | Improvement | Why |
|------|-------------|-----|
| 2026-07-02 | Preserve `prior_output` for live `/execute` (not full replace) | Review caught regression: streaming progress wiped chat |
| 2026-07-02 | `!cmd` uses same y/n gate as scope blocks + streaming | Ungated shell was strongest trust-boundary gap |
| 2026-07-02 | `ExecuteProgress::with_cancel` between scopes | Frozen loop had no in-TUI escape during partition |
| 2026-07-02 | `ENV_TEST_LOCK` for `COXN_VIM` / `COXN_AUTO_APPROVE` tests | Parallel test flake from process-global env |
| 2026-07-02 | `scratch/gated-loop/` tracking | User asked for improving loop with memory of what works |

| 2026-07-02 | `partition cancelled` appends to transcript | Pass-2 P3: decline confirm no longer wipes chat |
| 2026-07-03 | on_idle + drain_input_edits + CancelTrack for responsive streams (incl. CLI piggybacks) | Typing, history recall (empty-input Up), scroll, and Ctrl-C now work while model/CLI streams without freezing the loop |

## Not yet in loop (candidates)

- Poll cancel during single-scope `pump.run_turn_streaming` (needs TurnIo hook) — partial: hooks landed, drive integration for normal turns improved
- Shared global `ENV_TEST_LOCK` across modules (vs per-module mutexes)
- O(n²) execute progress snapshots on huge merged upstream — throttle or diff-append
- Usage reporting from grok streaming-json (end event has no tokens; claude result/assistant usage parsed)
- Live end-to-end model partition with role routing (/execute scout/synth) — hermetic route tests landed; live LLM+aden smoke still open