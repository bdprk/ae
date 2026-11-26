## What changed

## Why

## Checks

- [ ] `cargo test --workspace --locked` passes
- [ ] `python scripts/verify_generated_c.py` passes (add `--sanitize` on Linux)
- [ ] Generated C is byte-identical unless this change intends to alter it
- [ ] `docs/current-language.md` updated if language behavior changed
- [ ] An ADR is added under `docs/adr/` if this is a long-term design decision
- [ ] Claimed ledger IDs are recorded in `docs/implementation-ledger.md`
