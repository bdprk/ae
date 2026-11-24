# Errors and Result Enums Specification (current implementation)

Status: describes the implemented language (2026-11-15). The M1.11 target (`Result[T, E]` as a generic, a `std::Error` trait, `.context()`, removing the pre-defined result enum families) is future work; this document owns what exists today. The `?` propagation mechanics live in `spec/patterns.md` §5 — this document owns the *values*.

## 1. Design Rules

### Hard Rules:
1. **One Error Mechanism**: fallibility is expressed as an enum value with a success/failure shape; there are no exceptions, no `panic`/`catch`, and no recoverable runtime errors (runtime failures terminate — see `docs/resource-model.md` §6).
2. **Failure Does Not Terminate**: a fallible built-in returns its result enum with the error description in the `Err` payload; only the *infallible* twins (`read_file`/`write_file` vs `try_read_file`/`try_write_file`) terminate on failure.
3. **Error Text Is Descriptive, Not Stable**: failure payloads vary with the platform's `strerror` localization; programs must not match on exact error text.

### Non-Goals (this version):
- No generic `Result[T, E]`: the nine preset families and shape-compatible custom enums are the whole surface.
- No user-facing error trait, wrapping, error codes, or `.context()`. A *model* of the M1.11 target trait lives in `std/core/error.ae` (the self-hosting data layer) and is exercised through generic trait-bound dispatch in `tests/run/error_trait.ae`; it is not a language-level error mechanism yet.

## 2. The Preset Result Enums

Nine enums are public to all files (no import required), with fixed variant order, numbered 0–8:

| # | Enum | Ok payload | Err payload |
|---|---|---|---|
| 0 | `ReadResult` | `string` | `string` |
| 1 | `WriteResult` | `i64` (bytes written) | `string` |
| 2 | `IntResult` | `i64` | `string` |
| 3 | `DirResult` | `vec[string]` | `string` |
| 4 | `FileResult` | `i64, i64, bool` (size, mtime seconds, is-dir) | `string` |
| 5 | `StatusResult` | (no payload) | `string` |
| 6 | `BoolResult` | `bool` | `string` |
| 7 | `CommandResult` | `i64, string` (exit code, output) | `string` |
| 8 | `FloatResult` | `f64` | `string` |

- Layouts are emitted **on demand** (ADR-66): only result types actually used by the program enter the generated C code; the numbering exists for layout selection, not for user-visible ordering.
- Values are ordinary enum values: bind with `let`, unwrap with `match` (`spec/patterns.md` §4); the enum cannot be `print`ed or compared whole.

## 3. Fallible Built-ins

| Family | Built-ins |
|---|---|
| File | `try_read_file`, `try_write_file`, `try_append_file` (→ `ReadResult`/`WriteResult`) |
| Directory | `try_list_dir` (→ `DirResult`) |
| Metadata | `try_file_info` (→ `FileResult`), `path_exists`, `path_is_dir` (→ `bool`) |
| Organization | `try_copy_file`, `try_rename_path`, `try_remove_file`, `try_create_dir`, `try_remove_dir` (→ `StatusResult`) |
| Text | `parse_int`, `to_string` (→ `IntResult`), `find` (→ `IntResult`), `to_i64` (→ `IntResult`), `to_f64` (→ `FloatResult`) |
| Environment | `env`, `cwd`, `read_stdin` (→ `ReadResult`), `set_cwd` (→ `StatusResult`) |
| Process | `run_command` (→ `CommandResult`) |
| Map | `map_get` (→ `IntResult` for `map[string, i64]`) |

Specifics pinned elsewhere in the docs: `env` distinguishes "exists but empty" (`Ok("")`) from "not set" (`Err`); `parse_int` is strict decimal with the full string consumed; `find` searches by byte index with an explicit length (embedded NULs are ordinary bytes); path algebra functions never touch the filesystem.

## 4. Custom Result Enums and the Propagatable Shapes

A custom enum participates in the error mechanism when it matches one of two shapes, with fixed variant names and order:

- `{ Ok(...), Err(string) }` — the shape of all nine preset families, and of the prelude `Result[T, string]` instantiation.
- `{ Some(T), None }` — the shape of the preset `Option[T]` (also public to all files without import).

The success payload may be any owned type (`string`, `vec`, struct, enum); the failure path releases it per the type's release routine when dropped. `Result[T, E]` itself is predefined (ADR-0233): `Result[i64, string]` propagates like any preset family. `ReadResult`, `IntResult`, `DirResult`, `StatusResult`, `BoolResult` and `FloatResult` are the `Result[string, string]`, `Result[i64, string]`, `Result[vec[string], string]`, `Result[(), string]`, `Result[bool, string]` and `Result[f64, string]` instantiations; `WriteResult` shares the `IntResult` instantiation. Either spelling names the same type, and a pattern may use either declaration name. A variant whose single payload is `()` writes the arm without bindings (`Result::Ok =>`); a `let` binding on `?` is rejected because there is no value to bind.

## 5. Boundary Rules

- One function propagates **one family**: `?` requires the enclosing function to return the **same** enum type it propagates; crossing families (e.g. `DirResult` from `try_list_dir` and `StatusResult` from `try_copy_file`) needs a `match` at the boundary.
- `?` cannot appear in `main` (nothing above it to return to); `main` also cannot be generic.
- A bare `call();` statement **must** end with `?` to discard a propagatable result — there is no syntax for silently discarding return values otherwise.

## 6. Test Matrix

| # | Case | Expected |
|---|---|---|
| E1 | `try_read_file` miss unwrapped by `match` | `Err(reason)` arm runs, no termination |
| E2 | `let x = parse_int(text)?;` in a function returning `IntResult` | Propagates `Err` as-is; releases live owned values first |
| E3 | Bare `try_list_dir(p)?;` statement | Payload released, execution continues |
| E4 | Custom enum `{ Ok(string), Err(string) }` used with `?` | Propagates like a preset family |
| E5 | `?` propagating a family different from the function's return type | Rejected |
| E6 | Two families propagated in one function | Rejected (one family per function) |
| E7 | `?` in `main` | Rejected |
| E8 | Bare `f();` discarding a propagatable result without `?` | Rejected |
| E9 | Only the used preset result enums appear in the generated C | Layout on demand (byte-size sanity) |
| E10 | Allocation failure on the `Err` text path reports "out of memory" and exits | Terminal per `resource-model.md` §6 |

See `examples/option_propagate.ae`, `examples/file_tidy.ae`, `tests/run/option_prelude.ae`, `tests/run/error_trait.ae`, and the `ui` snapshots for the `?` rejections.
