# ADR-0243: The pure MIR move/borrow checker becomes the authority

- Status: Proposed
- Date: 2026-11-27
- Associated Milestone: P3.3 (unified move/borrow checking)

## Context

Acceptance has two independent move/borrow implementations today. The
frontend's `movewalk`/borrow passes run during lowering and decide what the
language accepts; the pure MIR pipeline (`mir_pure` lowering plus
`ae_mir::borrowck`) re-checks the lowered body and must agree. ADR-0237
calibrates the second against the first over `tests/run`: every fixture the
legacy compiler accepts must lower, validate and borrow-check, with only the
documented unsupported shapes skipped.

That calibration is exactly that — a check, not the authority. When the two
disagree, the legacy pass wins, because it runs first and produces the
diagnostic. P3.3's destination is the opposite: one move/borrow model over
`ae_mir::Body`, field-granular and NLL-based, with the frontend lowering
carrying only structure.

The pure pipeline now covers the whole language surface the corpus and
examples exercise (164/164 run fixtures, 49/49 examples, both fuzz generators
at 300 seeds with zero skips). The remaining distance is not feature
coverage but authority: diagnostics, the check route, and the retirement of
the duplicate passes.

## Options

| Option | Pros | Cons | Cost |
|---|---|---|---|
| Keep calibration only | no risk, two opinions | the second implementation never governs; divergences stay latent | low |
| Run both checkers on every compile (belt and braces) | catches bugs in either | double diagnostics, unclear which is authoritative, slower | medium |
| Make the MIR checker authoritative and delete the legacy pass (chosen) | one model, one diagnostic source, P3.3 complete | needs message/position parity first; a wrong MIR checker now rejects valid programs | high, staged |

## Decision

Move authority to the pure MIR checker in stages, each stage behind the
existing gates:

1. **Zero skips.** The calibration gate must report every accepted fixture as
   checked, with no `pure MIR lowering cannot handle` skip. Any shape the
   pure lowering still rejects is either implemented or explicitly removed
   from the accepted corpus with an ADR. The skip allowance in ADR-0237 is
   retired at that point.
2. **Diagnostic parity.** The MIR checker's rejections carry the same stable
   codes (`E0101`/`E0102`/`E0103`/`E0104` and the borrow-specific codes) and
   the same positioned spans as the legacy pass. A new gate runs the
   negative fixtures (`tests/ui`, `tests/types`, `tests/resolve` rejections)
   through the pure check route and compares code + range.
3. **Route switch.** `check`/`build`/`run` accept a program only after
   `compile_path_pure_mir_bodies` (lower → optimize → validate →
   borrow-check) succeeds. The legacy move/borrow passes stop deciding; they
   may remain as debug assertions for one release, reporting a mismatch as
   an internal error rather than a user diagnostic.
4. **Retirement.** Delete the legacy decision points once the routes agree
   on the corpus, the examples and both fuzz modes; keep the calibration
   test as a regression over the historical corpus.
5. **Fail closed.** A shape the MIR checker cannot model yet is an
   unsupported-subset skip with a named message, never a silent acceptance.

## Evidence

Stage 1 is met: `tests/pure_borrowck_calibration.rs` now requires **165
checked / 0 skips** (`skipped.is_empty()`), and every one of the 164 run
fixtures lowers, validates and borrow-checks.

Stage 2 is met: `tests/pure_diagnostic_parity.rs` runs all 137 negative
fixtures (`tests/ui`, `tests/types`, `tests/resolve`, `tests/hir`) through
both routes and requires the identical message and position, plus a
positioned diagnostic for every rejection — zero divergences. The acceptance survey is widened beyond the run corpus:
`tests/pure_route_coverage.rs` lowers, validates and borrow-checks every
accepted program under `examples/` and `std/` (50 programs), so the route
switch cannot reject an example or library module. To be
recorded per later stage:
the workspace suite and clippy on both hosts, the corpus and examples gates,
and the fuzz summaries for both generators. This ADR stays Proposed until
stage 3 lands.

## Consequences

- The language's error messages and acceptance become properties of the MIR
  checker; the frontend lowering keeps only syntax and type structure.
- A regression in the MIR checker now rejects valid programs instead of
  being caught by the legacy pass, so stage 1-2 gates must be strong before
  the route switch.
- The move/borrow logic exists once (in `ae_mir`), which is also the form a
  future native backend consumes; P6 does not re-implement it.
- Rollback: keep the legacy passes dormant (stage 3's debug assertions) for
  one release; re-enabling them is a configuration change, not a rewrite.