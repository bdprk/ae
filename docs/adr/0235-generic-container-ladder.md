# ADR-0235: Generic container ladder (`[T; N]`, `&[T]`, `Vec[T]`, `Map[K, V]`)

- Status: Proposed
- Date: 2026-11-25
- Associated Milestone: P2.4 (Hard Rule 1; audit §7.4 F1), feeds P4.1

## Context

With the result-enum half of P2.4 closed (ADR-0233, ADR-0234), the remaining
Hard Rule 1 violation is the container surface. `TyKind` still carries
`Vec`, `StrVec`, `VecF`, `VecRef`, `StrVecRef`, `VecFRef`, `Map`, `MapF` and an
`i64`-only `Array(usize)` (`crates/ae-ty/src/concrete.rs`), the C runtime has
three hand-written vector families (`ae_vec`, `ae_strvec`, `ae_f64vec`) and
two map families, `vec[...]` literals require i64/f64 elements, and
`std/alloc/{vec,map,box}.ae` are non-functional stubs. A program cannot write
`Vec[i8]`, `Vec[Point]`, `Map[i64, String]` or a `[f64; 4]` array.

The lifetime model and ownership rules already exist: moves are structural,
`Drop`-carrying structs are released in reverse declaration order, `Copy`
structs copy, raw pointers and `ptr_read`/`ptr_write`/`ptr_drop_in_place` are
available, and `c_size_of`/`c_align_of` answer layouts. A generic container is
therefore expressible without new memory-safety primitives.

## Options

| Option | Pros | Cons | Resource Cost |
|---|---|---|---|
| A. Parametric compiler container + runtime header (`{data, len, cap, elem_size, drop}`), then migrate the corpus to stdlib `Vec[T]`/`Map[K,V]` and delete the builtin families | One type constructor for every element type; existing builtin operations keep working through the header; incremental, byte-comparable slices | Runtime indirection (elem_size/drop in the header); C accessors need casts; stdlib migration last | One header + typed accessors; one word each per vector |
| B. Keep the three vector families and add a fourth/fifth per new element type | No refactor | Repeats the Hard Rule 1 violation | Linear code growth |
| C. Jump straight to `std/alloc` AE implementation and delete the builtin `vec[...]` immediately | Purest stdlib | Loses the builtin operations before the stdlib can replace them; blocked on allocator traits (P4.1) | Highest risk, largest single change |

## Decision

Take option A, in this order.

1. **One container type constructor.** `TyKind::Vector(ElemId)` /
   `TyKind::VectorRef(ElemId)` replace the seven `Vec*`/`StrVec*`/`VecF*`
   variants; `ElemId` names an interned element type (starting with the three
   current ones, then any `Copy` scalar/struct, then owned types). `Map` /
   `MapF` become `MapOf(KeyId, ValueId)`. The C runtime keeps one
   `ae_vector { void *data; int64_t len; int64_t cap; int64_t elem_size;
   void (*drop)(void *); }` and one map header; the three C families become
   instantiations of it or thin aliases during the transition. `Type::Array`
   becomes `ArrayOf(ElemId, usize)`.
2. **Operations stay the same names.** `push`/`pop`/index/sort/reverse/len/
   `keys` lower through the header, so the corpus keeps compiling; sort needs a
   per-instantiation comparator (scalars and strings first, then
   `Ord`-carrying types). Existing generated C for i64/string/f64 programs is
   kept byte-identical where the header can be hidden behind typed accessors.
3. **Element kinds grow monotonically.** Slice 1: i64, string, f64 through the
   generic constructor (behavior identical). Slice 2: every `Copy` scalar and
   `Copy` struct (`Vec[i8]`, `Vec[bool]`, `Vec[Point]`, `Vec[fn(...)]`).
   Slice 3: owned structs/enums with drop glue (`Vec[String]`, `Vec[Shape]`).
4. **Arrays and slices.** `[T; N]` literals and `&[T; N]` parameters generalize
   over the element kind; `&[T]`/`&mut [T]` join as a fat pointer
   `(ptr, len)` with the existing borrow rules. Slices are the last rung
   because they need the borrow checker to see the fat pointer shape.
5. **Stdlib takeover.** Once the builtin families are generic, `std/alloc`
   implements `RawVec[T]`/`Vec[T]`/`Map[K, V]` over the allocator trait
   (P4.1), and the `vec[...]`/`map_new` builtins lower to the stdlib
   constructor. Only then are the intrinsic entries deleted from
   `docs/architecture/intrinsic-allowlist.txt`; the compiler keeps allocation
   and pointer primitives, not container semantics.

## Consequences

- The remaining P2.4 exit conditions become reachable slice by slice:
  zero hardcoded per-type container families at the type level (slice 1), then
  at the runtime level (slice 3/4), then zero container intrinsics (slice 5).
- `ElemId` needs an interner in `CompileContext` (like pointees/signatures) so
  `TyKind` stays `Copy`; structural equality of element types drives caching.
- Existing id-pinned dumps change where `vec`/`map` spellings printed:
  `vec` becomes `vec[i64]` (or stays as the canonical display for the default
  element), and the C temp names for vectors change only if the header rename
  is not hidden. Each slice pins its own deltas.
- Sort on generic elements needs a comparator; until traits land, only
  scalars/strings sort, and other element kinds reject with a diagnostic.
- Memory growth: the header adds one word per vector value; acceptable against
  the Charter's footprint budget and removed again by monomorphized stdlib
  types in slice 5.
## Implementation checklist (slice order)

1. **Element interner + type constructor.** `ae-ty` gains an element-type
   interner (`ElemId`, owned by `TypeInterner` like pointees/signatures) and
   `TyKind::Vector(ElemId)` / `TyKind::VectorRef(ElemId)`. Adding the variants
   turns every exhaustive `match Type` into a compile error, which is the
   migration list; each site maps the three current kinds to interned
   `i64`/`string`/`f64` elements first. Producers keep emitting the old
   variants behind one `canonical_vector` helper until their module migrates.
2. **Module-by-module conversion.** Lowering literals and builtins, MIR,
   codegen, interpreter, borrow scope, dumps; delete `Vec`/`StrVec`/`VecF`
   and their refs last with a byte-parity gate against the previous commit.
3. **Runtime header + typed accessors.** One `ae_vector` header and one map
   header; the three C families become accessors over it; `MapOf(K, V)`.
4. **Element kinds.** `Copy` scalars and `Copy` structs first, then owned
   structs/enums with per-instantiation drop glue.
5. **Arrays and slices.** `[T; N]` literals/parameters, then `&[T]`/`&mut [T]`
   fat pointers with borrow tracking.
6. **Stdlib takeover.** `std/alloc` `RawVec`/`Vec`/`Map` over the allocator
   trait, `vec[...]`/`map_new` lowering to the stdlib constructors, and the
   container entries deleted from the intrinsic allowlist.
