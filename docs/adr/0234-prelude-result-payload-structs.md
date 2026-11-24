# ADR-0234: Struct payloads for prelude result families and layout ordering

- Status: Proposed
- Date: 2026-11-24
- Associated Milestone: P2.4 (Hard Rule 1; audit §7.4 F1)

## Context

ADR-0233 migrated the flat-payload preset result families onto the prelude
generic `Result[T, string]`: `ReadResult`, `IntResult`/`WriteResult`,
`DirResult`, `StatusResult`, `BoolResult` and `FloatResult` are now
instantiations. Two families remain monomorphic:

- `FileResult { Ok(i64 size, i64 mtime, bool is_dir), Err(string) }`
- `CommandResult { Ok(i64 status, string output), Err(string) }`

`Result[T, string]` has a single `Ok` payload, so a multi-field success needs
a named product type. The generic-enum argument rule also rejects struct
arguments today ("no recursion and no hidden layout"), and the C backend emits
enum layouts before struct layouts, so an enum payload of struct type would
name a not-yet-defined `ae_t{k}`.

## Options

| Option | Pros | Cons | Resource Cost |
|---|---|---|---|
| A. Named prelude payload structs + struct arguments for the seeded prelude instantiations | One `Result` shape for every builtin; source keeps the family names and gains struct field access; no new language surface for users | The C runtime's initializers and the corpus patterns that bind three/five fields change; layout emission needs a dependency order | Two prelude structs; one layout-order refactor |
| B. Tuple payloads (`Ok(i64, i64, bool)`) in generic enums | No named types | New anonymous product type in the language, layout, patterns and runtime; larger than the problem | New type kind across the pipeline |
| C. Keep `FileResult`/`CommandResult` as hand-written enums | Zero change | P2.4's exit ("zero hardcoded monomorphic result enums") is not met | None now, permanent carve-out |

## Decision

Take option A for the remaining two families.

1. **Prelude payload structs.** `FileInfo { size: i64, mtime: i64, is_dir:
   bool }` and `CommandOutput { status: i64, output: string }` are registered
   after the user's structs when a compilation loads, so existing user struct
   ids (`ae_t0..`) do not shift. Their layouts are emitted in the pre-runtime
   block, immediately before the prelude enum layouts that reference them.
2. **Seeded instantiations.** At program assembly the prelude `Result`
   generic is instantiated with `[FileInfo, string]` and `[CommandOutput,
   string]`; the `done`/`enum_origin` entries point at the existing
   `FileResult`/`CommandResult` ids. The two enum definitions keep their names
   (and become aliases of `Result` for patterns), so the families are
   instantiations rather than hand-written payload shapes. `FileResult::Ok` /
   `CommandResult::Ok` now bind one product value: `Ok(info)` and
   `Ok(result)`, with fields read as `info.size`, `result.output`.
3. **Layout ordering.** C layout emission is ordered by dependency: a struct
   is emitted before an enum whose payload names it, and an enum before a
   struct whose field names it. Programs without cross-kind payloads keep the
   current order (enums before structs), preserving byte-identical C.
   By-value cycles are rejected with a diagnostic (they are unrepresentable in
   C); pointer payloads would break the cycle and are out of scope here.
4. **Runtime macros.** The `AE_FILE_*` / `AE_COMMAND_*` initializers change to
   the struct-member form; ids stay stable because both families remain at
   their registration positions.

## Consequences

- P2.4's result-enum half closes: zero hand-written payload-specific result
  enums; the remaining container families (`Vec`, `StrVec`, `VecF`, `Map`,
  `MapF`, `Array`) are the next ladder.
- Examples and tests that destructure the two families change spelling once
  (`Ok(size, mtime, is_dir)` → `Ok(info)` + field reads); `spec/errors.md`
  documents it.
- User source cannot yet *name* `FileInfo`/`CommandOutput` (prelude struct
  names are registered after parsing). Naming them, and generic struct
  arguments for user enums, is a later slice; the compiler-internal
  instantiation is enough for the builtin migration.