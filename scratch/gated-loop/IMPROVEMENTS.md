# Loop improvements discovered in review

| Date | Improvement | Why |
|------|-------------|-----|
| 2026-07-02 | Preserve `prior_output` for live `/execute` (not full replace) | Review caught regression: streaming progress wiped chat |
| 2026-07-02 | `!cmd` uses same y/n gate as scope blocks + streaming | Ungated shell was strongest trust-boundary gap |
| 2026-07-02 | `ExecuteProgress::with_cancel` between scopes | Frozen loop had no in-TUI escape during partition |
| 2026-07-02 | `ENV_TEST_LOCK` for `COXN_VIM` / `COXN_AUTO_APPROVE` tests | Parallel test flake from process-global env |
| 2026-07-02 | `scratch/gated-loop/` tracking | User asked for improving loop with memory of what works |

| 2026-07-02 | `partition cancelled` appends to transcript | Pass-2 P3: decline confirm no longer wipes chat |

## Not yet in loop (candidates)

- Poll cancel during single-scope `pump.run_turn_streaming` (needs TurnIo hook)
- Shared global `ENV_TEST_LOCK` across modules (vs per-module mutexes)
- O(n²) execute progress snapshots on huge merged upstream — throttle or diff-append