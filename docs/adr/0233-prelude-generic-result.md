# ADR-0233: Prelude generic `Result[T, E]` and the container-genericity ladder

- Status: Proposed
- Date: 2026-11-24
- Associated Milestone: P2.4 (Hard Rule 1; audit §7.4 F1)

## Context

Hard Rule 1 forbids per-type container families and pre-defined result enums.
The audit (2026-11-22) found both: `TyKind` carries `Vec`, `StrVec`, `VecF`,
`VecRef`, `StrVecRef`, `VecFRef`, `Map`, `MapF` and an `i64`-only `Array(usize)`
(`crates/ae-ty/src/concrete.rs`), and the compiler prelude registers nine
payload-specific result enums (`prelude_enums()` in
`crates/ae-compiler/src/frontend/mod.rs`) whose C layouts runtime_ex.c names
`ae_e0`..`ae_e8`.

`Option[T]` has been a prelude generic since M1.2 (`prelude_generics`), and the
generic-enum machinery (construction, `match`, `?`, monomorphized C layouts,
interpreter values) handles arbitrary instantiations for the flat payload
types it admits. `Result[T, E]` is documented in `spec/types.md` and
`spec/stdlib.md` but only exists as hand-written AE source in
`std/core/result.ae`, so no program can use it without declaring or importing
it, and every fallible built-in keeps its own monomorphic enum.

## Options

| Option | Pros | Cons | Resource Cost |
|---|---|---|---|
| A. Register `Result[T, E]` in `prelude_generics` beside `Option[T]` | One generic fallible shape available everywhere; reuses the proven generic-enum path; no behavior change for existing programs | Adds a second prelude generic definition (ids are internal) | One enum declaration; instances are monomorphized as today |
| B. Import `Result` from `std/core/result.ae` | Keeps the compiler prelude minimal | Requires an import for every function returning a result, unlike `Option`; a missing search path today makes it unreachable | Module resolution work before any benefit |
| C. Keep the nine payload-specific prelude enums | Zero change | Directly violates Hard Rule 1; new payload types keep adding enum families | None now, linear cost later |

## Decision

Take option A for this slice, and stage the rest of P2.4 behind it.

1. **Prelude `Result[T, E]`.** `prelude_generics()` registers
   `Result[T, E] { Ok(T), Err(E) }` next to `Option[T]`; the resolver's
   `PRELUDE_TYPES` gains `Result` so a dump classifies it as a prelude type.
   Every program can now write `Result[i64, string]`, construct
   `Result[i64, string]::Ok(…)`, match with `Result::Ok(…)` (patterns name the
   declaration, `spec/patterns.md`), and propagate with `?` when the enclosing
   function returns the same instantiation and the error payload is `string`
   (the existing propagatable-shape rule, `spec/errors.md` §4).
2. **Instantiation arguments stay flat.** Generic-enum arguments remain the
   payload-capable flat set (`i64`, `bool`, `f64`, `char`, `string`, `vec`,
   `vec[string]`, `fn(…)`); a struct or enum argument is still rejected with
   the existing `type arguments must be …` diagnostic. Nesting enum payloads
   (`Result[Option[i64], string]`) needs recursive layout and is a later
   slice.
3. **Migrate the nine preset result enums onto instantiations.** Each
   `Ok`-payload becomes the `T` of `Result[T, string]`; two families carry
   multi-field `Ok` payloads (`FileResult` = size, mtime, is_dir;
   `CommandResult` = status, output) and first become named prelude payload
   structs so `Result[FileInfo, string]` and `Result[CommandOutput, string]`
   can express them. The C runtime's `AE_<NAME>_RESULT` macros then point at
   compiler-emitted aliases (`ae_e{eid}` of the instantiation) instead of the
   hard-coded `ae_e0`..`ae_e8`. `prelude_enums()` and the runtime's nine
   macro blocks are deleted in that slice.
4. **Containers follow the same ladder.** `Vec[T]`, `Map[K, V]`, fixed
   arrays `[T; N]` and slices `&[T]` become prelude generic declarations over
   the raw allocation and pointer primitives (`alloc`, `ptr_write`,
   `ptr_drop_in_place`, `c_size_of`, `c_align_of`) that P2.1–P2.3 made
   expressible, and each element family (`i64`, `string`, `f64`) migrates
   behind a byte-identical step until `TyKind::Vec`/`StrVec`/`VecF`/`Map`/
   `MapF`/`Array` can be deleted. `vec[...]` literal syntax lowers to the
   generic constructor in the final step.

## Consequences

- Programs gain the prelude `Result` without a declaration or import; the
  generated C for existing programs is unchanged because no builtin has been
  migrated yet (the new definition only becomes an instance when a program
  names it).
- The legacy prelude enum names keep working during the migration, so the
  corpus and examples are not rewritten in one step.
- `Result` participates in the existing instantiation budget (256 enum
  types per program) and the existing `?` shape rules; no new runtime type
  information is introduced.
- Exit gate for the whole item remains P2.4: zero hardcoded per-type
  container families and zero hardcoded monomorphic result enums.