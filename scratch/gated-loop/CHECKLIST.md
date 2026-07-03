# Per-pass checklist (coxn)

Run after each meaningful diff batch, before continuing feature work.

## Health stack (always)

- [ ] `cargo fmt --check`
- [ ] `cargo clippy -- -D warnings`
- [ ] `cargo test`
- [ ] `bash scripts/check-no-secrets.sh`

## Trust / safety (coxn-specific)

- [ ] New shell paths go through human gate (y/n or tool approval modal)
- [ ] User-initiated `!cmd` documents NO SANDBOX when bwrap absent
- [ ] Long-running local ops poll `poll_user_cancel()` (streaming or between steps)
- [ ] Slash commands that repaint `view.output` preserve session context when appropriate
- [ ] Sub-agent `/execute` auto-approve documented as partition threat model

## UX / TUI

- [ ] Chat-first copy matches bindings (`?`, `mode: CHAT`, `!cmd`)
- [ ] Help overlay and README agree on risk disclosure
- [ ] Live progress does not destroy transcript
- [ ] Empty input: Up recalls prior prompt; Down returns through history (typing scrolls chat)
- [ ] Shell ergonomics spot-check: history, submit, scroll — no surprise rebinding

## Telemetry / cross-repo cosmetics

- [ ] User-facing estimates tagged `[est.]` (not presented as measured)
- [ ] `savings.json` ledger math validated in tests (`baseline - returned`)
- [ ] aden read tools that coxn uses all record to the savings ledger (grep/understand/locate/asm/ask)
- [ ] Refresh cadence documented: cosmetic probes only after turns/slash, never per redraw frame

## TUI hot path (perf)

- [ ] No blocking subprocess / `Command::output` in `drive()` loop body (grep `drive.rs` loop)
- [ ] 60s idle dogfood: zero `aden` spawns while TUI sits at prompt

## TUI 3.0 (`COXN_TUI3=1`)

- [ ] Chrome shows model + scope without scrolling conversation
- [ ] `/execute` and slash listings land in activity drawer only
- [ ] Conversation cards distinguish you / coxn / tool in <3s scan
- [ ] `/copy` exports conversation + activity via `export_text()`
- [ ] `Ctrl-T` / `Ctrl-Shift-R` toggle tools collapse and reasoning hide

## Tests

- [ ] No parallel `set_var`/`remove_var` on process env without lock
- [ ] New behavior has unit test where cheap (progress callback, cancel, formatting)

## Loop meta

- [ ] Log pass in `scratch/gated-loop/LOG.md`
- [ ] Add new gate items to this file when review finds a repeatable miss