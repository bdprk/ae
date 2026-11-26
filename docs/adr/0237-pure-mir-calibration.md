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

## Builtin intrinsics travel with the body

Intrinsic calls are synthetic `FnDef` ids (a deterministic FNV-1a hash in the
high-bit range), which a standalone backend cannot resolve through a source
function map. `Body` now carries `intrinsics: BTreeMap<DefId, String>` filled
by the lowering, and `ae-codegen-c` resolves those ids through it. `print`
and `exit` are implemented in the backend: `print` selects the printer from
the operand type (i64/u64/bool/char/string; the canonical f64 text formatter
is ported from the legacy runtime so fixture stdout compares byte-for-byte)
and `exit` calls a `_Noreturn ae_exit`. Every other intrinsic fails closed
with `intrinsic '<name>' is not supported by the pure C backend yet`.

Two supporting fixes made the corpus comparable: string constants are emitted
raw into the C literal (the AE escape set is a subset of C's, and the legacy
backend does the same, so both produce the same bytes), and `Deref` type
inference maps `StructRef`/`StructMut`/`VectorRef` to the pointee, so field
reads through `&Struct`/`&mut Struct` compile.

## End-to-end corpus gate

`tests/pure_e2e_corpus_gate.rs` emits, compiles with the host C compiler
(`-std=c11`), runs and compares stdout for every accepted `tests/run` fixture
whose emission succeeds. At this slice 90 of 161 fixtures match byte-for-byte;
the floor is 85 and rises as coverage grows, and any output mismatch for a
fixture that ran is a hard failure. Layout probes (`c_size_of`,
`c_align_of`, `c_offset_of`) run but are excluded from the comparison until
`ae-layout` (P6.1) drives this backend's layout; the remaining emission gaps
are the unimplemented intrinsic families (vectors, strings, threads, channels,
atomics) and five C-emission bugs (array element typing, enum variant naming,
function-pointer casts).

## Layout metadata travels on the body

Two C-emission bugs came from the backend guessing type metadata.
`AggregateKind::Array` carried only the element type, so two same-length
arrays of different element kinds shared one type slot (an i64 array could be
emitted as a Copy-struct array); the aggregate now carries the compiler's
interned element id. A match arm binds a payload variant the body may never
construct, so the enum union typedef lacked that member (`u.v1` missing while
`v10` existed); `Body.enum_layouts` now carries every declared variant's
field types and `ae-codegen-c` seeds its enum table from it. The end-to-end
gate rises from 90 to **93 of 161** fixtures with output parity intact.

## Pointer metadata and indirect calls

Two more type facts travel on the body so a standalone backend can type
operations with no syntactic type. `Body.pointees` maps a raw pointer's
interned pointee id to its type (collected through containers, struct fields
and enum payloads; the lookup is bounds-checked because a few lowering temps
carry `PtrMut(0)` placeholders), and `Body.fn_sigs` maps a function-signature
id to `(params, return)` (collected from locals and from `FnRef`/`FnPtr`
constants). `ae-codegen-c` emits one
`typedef ret (*ae_fn_t{sig})(params);` per signature, spells a function value
as that typedef, casts a function designator to it, and calls a
function-typed binding through the operand itself. `FnCall` now lowers to an
indirect callee (the slot operand) rather than a direct call to a
same-numbered function: the variable and function id spaces are separate.

The optimizer also gained a soundness fix: intra-block constant propagation
clears its environment on a store whose projection contains `Deref`. A raw
pointer write may land in any local whose address was taken, so folding a
later read of that local back to its pre-write constant is wrong (the corpus
caught exactly that in `unsafe_pointer_write`). The end-to-end gate rises
from 93 to **97 of 161** fixtures.
