# ADR-0236: `&[T]` slices as borrowed fat pointers

- Status: Accepted (rungs 1, 2 and 3 landed 2026-11-25)
- Date: 2026-11-25
- Associated Milestone: P2.4 action 1 (generic `Slice[T]`), feeds P4.1/P7.1

## Context

ADR-0235's last language rung is slices. The language already has two borrowed
contiguous views: `&vec[T]` (a whole vector, the storage's own length) and
`&[T; N]` (a whole fixed array, length in the type). Neither accepts the other,
so every helper is written twice (`fn sum(v: &vec)` vs `fn sumN(a: &[i64; 3])`),
and a function cannot take "some contiguous `T`s" — the shape Go's `[]T` and
Rust's `&[T]` exist for. `vec[T]` is generic since ADR-0235, so the element type
is available for a generic slice.

Constraints from the existing design:

- A view has no storage of its own; the borrow tracker keeps the owner alive
  and blocks writes while a view can still be read (ADR-0205's NLL regions).
- Element reads follow the storage kind: Copy elements read as values, strings
  as `StrView`, nested vectors as `&vec`, owned structs as `&Struct`.
- The interpreter models borrows by referencing the owner's frame
  (`Val::Ref`/`Val::Ptr`), not raw addresses.
- The C backend is a single translation unit with a pruned runtime; a new
  runtime type costs bytes only when used (Hard Rule 8).

## Options

| Option | Pros | Cons | Resource Cost |
|---|---|---|---|
| A. `&[T]` is a fat pointer `{ const void *data; int64_t len; int64_t elem_size; }`, constructed at call sites from a vector or an array | One type for "any contiguous run"; works for every element kind; C is one struct + two accessors; the length travels with the pointer | 24 bytes per slice value (by value at calls); indexing goes through `memcpy`-style accessors | One 3-word struct + 2 accessors |
| B. Emit a per-element slice struct family (`ae_slice_i64`, `ae_strvec_view`, …) | Typed accessors, no elem_size word | Repeats the Hard Rule 1 violation the vector ladder just removed | Linear growth per element type |
| C. Only a `&vec[T]` alias without a length view (no arrays) | Smallest patch | Does not satisfy "some contiguous `T`s"; arrays keep their own path | Small but fails the goal |
| D. Sub-slicing expressions (`v[a..b]`) in the same rung | Most expressive | Needs a range grammar, runtime offset arithmetic, and a second borrow shape in the same slice | Larger, riskier |

## Decision

Take option A, in rungs.

1. **Type and representation.** `TyKind::Slice(ElemId)` names a read-only view
   of contiguous `ElemId` values; the C representation is
   `typedef struct { const void *data; int64_t len; int64_t elem_size; } ae_slice;`.
   The element is interned exactly like a vector's (`intern_slice_elem`), so
   structural equality drives monomorphization and the display spelling is
   `&[elem]`. `&mut [T]` is not part of rung 1.
2. **Spelling.** `&[T]` in parameter and callable-parameter position; `&[T; N]`
   keeps its length and remains the fixed-array form. A local cannot bind a
   slice directly (it is a borrow; `let s: &[i64] = …` is rejected like `&vec`),
   but a slice parameter may be forwarded to another slice parameter.
3. **Construction (call-site coercion, rung 1).** An argument for a `&[T]`
   parameter may be:
   - a whole vector binding of element `T` — `sum(v)` passes `(v.data, v.len,
     sizeof(T))`, registering a call-duration borrow exactly like `&v`;
   - a whole array binding of element `T` and length `N` — `sum(&arr)` passes
     `(arr, N, sizeof(T))`;
   - a slice parameter (`fn inner(s: &[T]) { return outer(s); }`), forwarded
     unchanged.
   Anything else (an element, a field, a temporary) is rejected with a pointed
   diagnostic; sub-slicing is rung 3.
4. **Operations.** `len(s)` returns the runtime length; `s[i]` bounds-checks and
   reads with the storage's element rules (Copy → value, `string` → `StrView`,
   `vec` → `&vec`, owned struct → `&Struct`); `for x in s` binds the element the
   same way the vector loop does and holds the owner frozen for the loop. Slices
   are read-only: pushes/stores/sorts over a slice are rejected.
5. **Interpreter.** `Val::Slice { owner, len }` names the owner slot in the
   caller's frame plus the visible length; reads go through the same
   `read_var`-style access `&vec` uses, so the differential oracle checks
   offset/length behavior without a second memory model. Passing a slice
   parameter to an inner call forwards the pair.
6. **Borrow tracking.** Building a slice from a vector is an `Address`-class
   borrow (the call freezes the vector for its duration), from an array the
   existing whole-array borrow, and from a slice parameter a `Forward`. A slice
   held by a `for` loop freezes the owner like the vector loop.

Rung 2 (element views and place borrows through a slice) and rung 3
(sub-slicing) are landed. `&mut [T]` (rung 4, with write tracking) remains,
as does the stdlib `Slice[T]`/`Iterator` takeover (P4.4).

### Rung 2 (landed): element place borrows

A borrow argument may name one element of a slice parameter — `&items[0]`,
`&rows[len(rows) - 1]` inside `fn f(items: &[Tag])` — and pass it to a
`&Struct`/`&vec` parameter. The place-borrow lowering resolves the index
against the slice's element type and emits `Expr::SliceIndex` under the
same `BorrowAs` the vector element borrow uses, so the callee reads the
owner's storage in place (`&(*((const ae_tN *)ae_slice_at(...)))`, the same
address-of-deref the vector form emits). The interpreter evaluates the
element read as a value, matching the vector element borrow's snapshot
model. A `string` element is not a place-borrow target (the same rejection
as the vector form), and an out-of-range index traps like any element read.
`tests/run/slice_element_borrows.ae` pins owned-struct and nested-vector
elements across the four backends.

### Rung 3 (landed): sub-slicing

`&v[start..end]`, `&a[start..end]` and `&s[start..end]` (a slice parameter)
window the fat pointer instead of forwarding the whole storage. The shared
grammar's `split_range_argument` finds a top-level `..` in a borrow
argument's brackets and returns the two bound byte ranges; the resolver
walks both as reads (liveness and borrow tracking see every variable a bound
names) and the lowering parses each bound as a scalar expression that must
be i64. Both bounds are required in this rung (`&v[1..]` is rejected with
"a slice range needs both bounds"). The C runtime's `ae_slice_sub` checks
`0 <= start <= end <= len` and traps at the call site ("slice range out of
bounds"), then offsets the data pointer and shrinks the length, so
`len`/index/`for` need no change. A full window (`start == 0 && end == len`)
returns the slice unchanged, which also keeps an empty slice's NULL data out
of pointer arithmetic. The interpreter validates the same bounds
against the same snapshot a whole slice copies, and the compiled and
interpreted paths trap with the identical message and position. A field
receiver (`&r.cells[0..1]`), a mutable range and an element slice remain
rejected with pointed diagnostics.

Generic slices compose with monomorphization: `fn size[T](v: &[T])` is a
valid parameter shape, and when the call spells its type arguments
(`size[i64](&v)`; inference is P2.5.2) `len`, `for` and sub-windows work on
each instantiation. `tests/run/generic_slices.ae` pins i64 and string
instantiations.

## Consequences

- One slice type serves arrays and vectors; helper duplication ends for reads.
- A slice value costs three machine words at a call; element access multiplies
  by `elem_size` instead of a typed load. Acceptable per the charter's footprint
  budget and removed again if the stdlib monomorphizes slices in P4.
- The element interner grows one table; `Type` gains one variant, which forces
  every exhaustive match to answer for it (the migration list).
- `&vec` does not disappear in this rung: it is still the cheapest whole-vector
  borrow for i64/string/f64 paths and stays the stdlib take-over target.
"""