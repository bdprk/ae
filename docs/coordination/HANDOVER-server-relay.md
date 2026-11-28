# Handover — server-relay

- Date: 2026-11-28 (+08:00)
- Branch: `agent/server-relay`, based on `9907ce26` (origin/main at
  session start; the branch carries only `docs/coordination/` commits)
- Worktree: `D:/用户/trevv/桌面/projects-bdprk-副号/worktrees/server-relay`
- Areas: docs (claimed `docs` for this agent in
  `.git/claims/docs__server-relay.json` — create it if missing; the
  PowerShell claim script cannot run on this machine, see SERVER-STATUS
  lessons)

## What is done

Server-side verification for ADR-0242 decision material, all evidence in
`docs/coordination/SERVER-STATUS.md` (branch commits `9f074156`,
`91421bdf`, `0df608fe`):

- evaluate-2 suite at `9907ce26`: 693/2/3 — the 2 failures diagnosed as
  stale `tests/execution.rs` pins against the f70a8c63 string-layout
  admissions (fixed upstream by slice 101).
- evaluate-2 suite at `9bfc9f86`: **695 passed / 0 failed / 3 ignored**,
  49/49 binaries, exit 0.
- Sanitizer leg: 41/41 legacy fixtures PASS at s1; 47 PASS + 1 harness
  bug at s2 (stale expected literal in
  `scripts/verify_generated_c.py:239` for heap_map.ae — one-line fix
  queued for the main agent). Five new fixtures characterized under
  ASan/LSan; rc_shared 16 B and generic_inference_composite 9 B are
  bisect-proven process-lifetime exemptions.
- Differential fuzz: s1 pure-subset 4752 seeds / 0 failures (retired
  early); s2 chain (pure 20k → mixed 20k) running unattended on the tip
  binary, ~84 seeds/min, 0 failures at last poll.

## What is half-done

- s2 fuzz totals below the 20k targets while the chain runs; totals and
  any failure excerpts must be appended to SERVER-STATUS.md and pushed.
- The s2 sanitizer rerun's heap_map case will keep asserting until the
  main agent fixes the literal.

## Next command

```sh
ssh instabox-node3 'tail -4 /data/home/ae-relay2/fuzz-pure.log; tail -4 /data/home/ae-relay2/fuzz-mixed.log; grep -E "PURE_EXIT|MIXED_EXIT" /data/home/ae-relay2/fuzz-chain.log'
```

then amend `docs/coordination/SERVER-STATUS.md` (fuzz table + sanitizer
note) and push `agent/server-relay`.

## Blockers

None. SSH certs renewed 2026-11-28 16:01 +08:00 (24 h) — renew with
`instabox ssh-cert -ttl 86400 evaluate-2` when they lapse.

## Files touched

- `docs/coordination/SERVER-STATUS.md` (new, three commits)
- `docs/coordination/HANDOVER-server-relay.md` (this file)
