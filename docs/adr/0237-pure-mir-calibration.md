# ADR-0237: Pure MIR pipeline — fail closed and calibrate the borrow checker

- Status: Accepted (calibration slice landed 2026-11-26)
- Date: 2026-11-26
- Associated Milestone: P3.1/P3.2 (pure MIR), P3.3 (unified move/borrow checking)

## Context

P3.1 lowered the compiler's CFG into the standalone `ae-mir` crate, P3.2 added
a from-scratch C11 backend over pure MIR (`ae-codegen-c`), and P3.3 added a
field-granular move/initialization analysis plus an NLL borrow checker that
runs on `ae_mir::Body` without the compiler's AST. The pipeline is exposed as
library entry points (`lower_path_to_pure_mir`,
`compile_path_pure_mir_bodies`, `compile_path_pure_mir_to_c`) with end-to-end
tests; it is not the default backend yet.

Calibrating the new borrow checker against the accepted `tests/run` corpus
surfaced three classes of divergence from the legacy checks. Each is a
semantic decision the pure pipeline must mirror before it can become the
authority.

## Decision

1. **Return releases run after the value is materialized.** The parser's
   scope releases ride `Return(value, frees)`; the legacy backends materialize
   the return value first and then run the releases (a moved value is
   forfeited, so its release is a no-op). In MIR the releases are trailing
   `Drop` statements in the return block, while the destination assignment is
   emitted at terminator-lowering time. The lowering now inserts the `_0 = …`
   assignment **before** the trailing run of `Drop` statements, so
   `return s` never drops `s` before copying it out.
2. **A `&place` stored into a raw pointer is an address, not a loan.** The
   language forms raw pointers through `&v as *const T` inside `unsafe`; the
   legacy checks treat that as a cast, not a borrow, so a later write to `v`
   is legal and visible through the pointer. `ae_mir::borrowck` now skips loan
   creation when the assignment's destination local is `Ptr`/`PtrMut`.
3. **Unsupported constructs fail closed.** Token cursors (`for x in tokens`,
   `ResetTokens`/`AdvanceTokens`) need a cursor runtime the pure C backend
   does not implement yet. The lowering previously emitted a `reset_tokens`
   call over an uninitialized iteration local. It now records the shape as
   unsupported and fails the whole lowering with a named message instead of
   emitting ill-formed MIR. Corpus gates skip exactly that documented message
   and bound the skip set (≤5 fixtures), so a new unsupported shape is a
   failure rather than a silent pass.

## Calibration gate

`tests/pure_borrowck_calibration.rs` runs
`compile_path_pure_mir_bodies` (lower → optimize → validate → borrow-check)
over every standalone `tests/run` fixture the legacy compiler accepts and
requires zero divergences outside the documented skip set. At this slice it
checks 159 fixtures with two token-cursor skips.

## Consequences

- The pure borrow checker agrees with the legacy checks on the accepted
  corpus; the remaining divergence surface is the documented token gap.
- Future pure-MIR work (strings/containers in `ae-codegen-c`, the interp
  rewrite) inherits a fail-closed lowering: an unimplemented shape is a named
  error, never a malformed body.
- Token cursor support in pure MIR is a later slice: it needs a cursor local
  initialized from `tokenize`, an advance step that writes the element, and
  matching `ae-codegen-c` runtime helpers.
- Raw-pointer writes stay outside the borrow checker by design (`unsafe`
  is the unchecked operation); the loan skip is limited to address casts, so
  a real `&T`/`&mut T` still freezes its place.