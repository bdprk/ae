# ADR-0239: Slices in the pure MIR C backend

- Status: Accepted
- Date: 2026-11-27
- Associated Milestone: P3.2 (pure MIR C backend parity), ADR-0236 (language slices)

## Context

ADR-0236 gave the language four slice rungs: `&[T]`, element place borrows,
sub-slicing `&v[a..b]`, and `&mut [T]` windows. The main backends implemented
them, but the pure C backend still failed closed on `slice_of`,
`slice_index`, `slice_range` and `Rvalue::Len` on a slice, which kept the
slice fixtures out of the end-to-end gate even after strings and vectors
landed.

Slices are views, not owners: the storage belongs to the vector or array that
was sliced, and the borrow checker already guarantees the base outlives the
view. The pure backend therefore needs a header plus accessors, not a fourth
ownership model.

## Decision

1. **One fat pointer for both slice kinds.** `typedef struct { void *data;
   int64_t len; int64_t elem_size; } ae_slice;` is the C type of `&[T]` and
   `&mut [T]`. A slice is a value; it is copied, never released, and never
   appears in a `Drop`.
2. **Element metadata travels with the body.** `Body.slice_elems` maps the
   compiler's slice interner id to the element type, so the backend can size
   an index, type an element read, and build a view without syntactic types.
3. **Construction.** `slice_of` builds `ae_slice_view(data, len,
   sizeof(T))` from a vector (value or `&vec`), an array (its inline `.data`
   plus the compile-time length), or another slice (header copy).
   `slice_range` applies `ae_slice_range(view, start, end)` with the same
   `array index out of bounds` trap as indexing. `Rvalue::Len` reads `.len`.
4. **Element reads follow the vector rules.** A scalar element is a copy; a
   string element is `ae_str_view` so the owner keeps the bytes; a struct
   element is a pointer for field chains (the frontend rejects binding an
   owned aggregate element); a nested-vector element is a header copy for
   read-only consumers.
5. **Mutable writes go through the place projection.** `ProjectionElem::Index`
   on a `SliceMut` place emits `*((T *)ae_slice_at_mut(&slice, index))`, so a
   window write is bounds-checked in the same place an array write is.
6. **A pure-MIR printer joins the CLI.** `ae emit-pure-mir` prints the graph
   the C backend consumes, so a lowering bug can be told apart from a backend
   bug. It exposed the slice argument shapes this ADR standardizes.

## Evidence

The end-to-end gate rises 126 → **132 of 161** accepted fixtures with
byte-identical stdout and zero tolerated mismatches. The WSL ASan+UBSan sweep
passes 134 of 136 emit-capable fixtures with no leaks or invalid accesses (the
two intended traps abort by design). Workspace suite 686 passed / 0 failed
(2 ignored), clippy clean, borrowck calibration unchanged.

## Consequences

- Slices add no runtime ownership: a missing release cannot leak a slice, and
  a slice cannot keep its base alive (that is the borrow checker's job).
- The mutable window shares the same header, so an accidental write through a
  read-only slice is a compile error in the frontend, not a silent mutation at
  runtime.
- The remaining slice fixtures are the ones that also need another family
  (sorting a vector field, externs, concurrency); the slice machinery itself
  is complete for the accepted corpus.