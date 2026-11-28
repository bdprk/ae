# ADR-0234: Struct payloads for prelude result families and layout ordering

- Status: Accepted (implemented in P2.4 slice 5)
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
   before any source is parsed, so their ids are the fixed `0` and `1` and
   user structs start at `2`. (The first draft appended them after the user
   structs to avoid an id shift, but the route scan reads a variant's payload
   shape while it parses; a post-load rewrite left the scan seeing the old
   multi-field shape. The two-id shift is accepted and the id-pinned dumps
   moved with it.) Their layouts are emitted in the pre-runtime block, inside
   the typedef piece of the prelude enum that stores them.
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
## Implementation notes (P2.4 slice 5)

- The route's field gate types a match payload binding from the variant's
  payload shape in its prepass (`lower_body`), and the permissive subset for
  a slot-less binding includes `string` fields (read as views), so
  `print(info.size)` / `print(result.output)` route like a parameter's field.
- The pruner scans a kept prelude typedef piece for symbols: the payload
  struct's release/forfeit helpers call the string primitives, which must
  survive the reachability walk even though the piece is not a named function
  unit.
- The generic-struct budget subtracts the two prelude structs, so a program's
  instantiation capacity is unchanged.
- The enum release/forfeit inlines the struct field releases (a copy struct
  zeroes; an owning struct calls its emitted helpers).
- Corpus updates: `examples/file_tidy.ae`, `examples/dir_report.ae`,
  `examples/run_tool.ae`, and the file/command execution tests bind the
  product payload (`Ok(info)`, `Ok(finished)`). `tests/ui/copy_derive_non_copy_field.stderr`
  and the MIR/tree/repr-C dump pins moved from `#0`/`ae_t0` to `#2`/`ae_t2`
  with the two-id shift.
