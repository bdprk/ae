# ADR-0238: Owned strings and containers in the pure MIR C backend

- Status: Accepted — stage 1 (owned strings) implemented 2026-11-27; stage 2 (vectors) pending
- Date: 2026-11-26
- Associated Milestone: P3.2 (pure MIR C backend parity), P4.1 (stdlib containers)

## Context

`ae-codegen-c` now emits, compiles and runs 97 of the 161 accepted `tests/run`
fixtures with byte-identical stdout (`tests/pure_e2e_corpus_gate.rs`). The
backend maps `string` to a plain `const char *` and has no container value
types: string literals are static byte sequences, `print`/`exit` are built in,
and everything else fails closed by intrinsic name. Two structural gaps keep
the remaining ~60 fixtures out:

1. **Owned strings.** `to_string`, `str_concat`, `slice`, `slice_view` are
   unimplemented because the backend's `const char *` cannot say whether a
   value owns its bytes. The MIR already carries `TerminatorKind::Drop`, but
   the emitter ignores it (a `Drop` is a plain `goto`), so a future allocating
   string would leak.
2. **Containers.** `vec_new`/`vec_push`/`vec_pop`/`vec_store`/`sorted`
   intrinsics and vector index projections are unimplemented; the backend's
   `TyKind::Vector` is an opaque `void *`.

The main backend already solved both shapes with one generic header each
(Hard Rule 1): `ae_string { ptr, len }` with allocation-provenance release, and
`ae_gvec { data, len, cap, elem_size, drop }` with per-element drop thunks.
The pure backend should adopt the same layouts rather than invent a second
container model, because P3.2's exit gate is byte-for-byte behavioral parity
and P4 replaces the runtime with AE-source containers on top of either
backend.

## Decision

Adopt the main runtime's layouts in the pure backend, in stages, each behind
the existing gate floor.

1. **`ae_str` value type.** `typedef struct { const char *ptr; int64_t len;
   bool owned; } ae_str;` replaces `const char *` for `string`/`&string`.
   - literals: `ptr` into the C literal, `len` computed at compile time,
     `owned = false`; `ae_str_len`, `ae_str_eq`, `ae_str_print` read both.
   - allocating operations (`to_string`, `str_concat`, `slice`,
     `slice_view`) return `owned = true` buffers from the backend allocator.
   - `TerminatorKind::Drop` on a `string` place becomes
     `if (place.owned) free((void *)place.ptr);` (and clears the flag), so
     every owned string releases exactly once on overwrite, early return and
     scope exit. `Drop` on non-owned places is a no-op.
   - `print`, `ae_fn_t` parameters, struct fields and enum payloads holding a
     string switch to `ae_str` in the same change; the ABI is internal to the
     backend, so the main runtime is untouched.
2. **`ae_vec` value type.** `typedef struct { void *data; int64_t len; int64_t
   cap; int64_t elem_size; void (*drop)(void *); } ae_vec;` — the same header
   the main backend uses. `vec_new` allocates zeroed capacity for a given
   element type (the intrinsic already carries the element `TyKind` operand),
   `vec_push`/`vec_pop`/`vec_store` operate through it, `Rvalue::Len` on a
   vector reads `len`, and `ProjectionElem::Index` on a vector emits a typed
   `((T *)data)[i]` with a bounds check. Owned element types get a generated
   drop thunk exactly as `ae_gvec` does today.
3. **Drops are emitted, not skipped.** The emitter's `Drop` terminator and
   `StatementKind::Drop` statement call a per-type release (`ae_str_release`,
   `ae_vec_release`, struct/enum walkers) before `goto`; until a type has a
   release, the backend fails closed instead of silently leaking.
4. **Intrinsic table, one name at a time.** `emit_intrinsic` grows a
   match arm per family (string first, then vector, then map); an arm that is
   not implemented keeps the current named `UnsupportedFeature`, so the gate
   floor only ever rises.
5. **Externs stay unsupported.** `extern "C"` calls still fail closed with a
   named error; linking the declarations is a separate slice once the
   backend ships its own runtime translation unit.

## Migration and evidence

- Stage 1 (strings) targets the six `str_concat`/`to_string`/`slice`/
  `slice_view`/string-`len` fixtures; stage 2 (vectors) targets the seven
  `vec_new` and vector-`len` fixtures; each stage:
  1. raises `PASS_FLOOR` in `tests/pure_e2e_corpus_gate.rs` to the new
     Windows count,
  2. adds a leak check for the pure backend's generated C (the gate compiles
     with the host compiler; a `--leak`-style wrapper or ASan run covers it),
  3. keeps zero tolerated stdout mismatches.
- `tests/pure_borrowck_calibration.rs` already exercises the same bodies, so
  the borrow checker stays the single authority regardless of the backend's
  progress.
- 100% `tests/run` + `examples/` parity and 50k fuzz seeds are the P3.2 exit
  gate; only then is the legacy tree emitter deleted.

## Stage 1 evidence (2026-11-27)

Stage 1 landed as one coherent slice:

- `ae_str { const char *ptr; int64_t len; bool owned; }` is the C type for
  both `string` and `&string`. A literal becomes a static value whose length
  is `sizeof("…") - 1` (escapes already decoded by the C compiler);
  `to_string`, `float_to_string`, `slice` and `str_concat` return owned
  buffers; `slice_view` and `string_view` clear the flag.
- Moves are explicit. A moving position (`let`, assignment, by-value call
  argument, struct/enum field, `return`, and each operand of `+`) lowers to
  `Operand::Move` when the type is owned and not `#[derive(Copy)]`; the
  emitter spells it `ae_str_take(&place)`, which copies the value and empties
  the slot. The conservative release lists therefore stay harmless: a moved
  slot releases nothing. `Body.copy_structs` / `Body.copy_enums` keep copy
  types copying.
- Drops are emitted. `StatementKind::Drop` and `TerminatorKind::Drop` on a
  string release it; every struct/enum gets `ae_rel_*` and `ae_take_*`
  helpers in topological order. A struct with a user `Drop` impl carries a
  live flag and calls the user function through `Body.drop_fns` before
  releasing fields, which makes a conservative second release a no-op.
- Owned temporaries read by `print` or a string comparison (a concatenation,
  a call result) are released after the read, closing the leaks the first
  gate run exposed.
- Function-pointer typedefs joined the topological type order, so a struct
  holding a fn pointer compiles; closures spell as `void *` matching the
  legacy backend's address constant.

Evidence: the end-to-end gate rises from 97 to **113 of 161** accepted
fixtures with byte-identical stdout and zero tolerated mismatches. An
ASan+UBSan sweep over every one of the 116 emit-capable fixtures in WSL
passes 115 with no leaks or invalid accesses (`divide_by_zero` traps by
design). Workspace suite 685 passed / 0 failed (2 ignored), clippy clean.

## Consequences

- The backend grows a small runtime (strings, vectors, their releases) that
  mirrors the main one; the duplication is transitional and dies with the
  legacy backend in P3.2.
- Layout probes (`c_size_of` etc.) remain excluded from the gate until
  `ae-layout` (P6.1) drives both backends, at which point they become a real
  comparison.
- Leaks become measurable: the gate's leak pass makes a missing release a
  failure, not a silent regression, which is the prerequisite for the P3.2
  exit gate.