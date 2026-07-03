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

## Tests

- [ ] No parallel `set_var`/`remove_var` on process env without lock
- [ ] New behavior has unit test where cheap (progress callback, cancel, formatting)

## Loop meta

- [ ] Log pass in `scratch/gated-loop/LOG.md`
- [ ] Add new gate items to this file when review finds a repeatable miss