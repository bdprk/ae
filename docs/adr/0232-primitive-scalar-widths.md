# ADR-0232: Primitive scalar widths, checked arithmetic and explicit conversions

- Status: Proposed
- Date: 2026-11-24
- Associated Milestone: P2.1 (see the master checklist §P2; closes the numeric surface of R2.4)

## Context

`spec/types.md` has promised the full primitive set since M1.1, but the
implemented frontend has one integer width: `TyKind::Int` is `i64`, `f64` is
the only float, and `char`/`()` do not exist. The C backend therefore emits
`int64_t`/`double`, the interpreter uses `i64`/`f64`, and every overload,
conversion and error message assumes that pair. Hard Rule 2 (integer overflow
traps in debug and release alike) is only honoured for the one width, and an
`i64`-only program cannot express a byte, a 32-bit counter or a Unicode scalar.

The master checklist's P2.1 exit requires all fifteen primitives
(`i8`, `i16`, `i32`, `i64`, `isize`, `u8`, `u16`, `u32`, `u64`, `usize`,
`f32`, `f64`, `bool`, `char`, `()`) to compile and run with identical traps on
the C backend and `--interp` on Windows and Linux. This ADR fixes the
semantics and the migration order; the implementation is staged below.

## Options

| Option | Pros | Cons | Resource Cost (Compile/Runtime/Memory/Disk) |
|---|---|---|---|
| A. Width-carrying `TyKind::Int(IntTy)` / `TyKind::Float(FloatTy)` / `Char` / `Unit`, migrated mechanically | One canonical IR for ty/check/MIR/codegen/interp; traps and ABI derivable from the type; future `i128` slots in | Touches every `Type::Int` match in the compiler; large migration | One-time code churn; no runtime cost |
| B. Keep `i64` and expose widths as library newtypes over it | Tiny frontend change | No real width semantics, no trap/bounds distinction, wrong C ABI for FFI, cheats the exit gate | Runtime wrapping cost; dishonest model |
| C. Widen in the frontend, erase widths before MIR | Smaller MIR change | MIR/codegen can no longer type constants, casts or overflow checks; interpreter and C backend diverge; hidden narrowing | Runtime bit-masking; debug/release drift risk |
| D. Suffix-based literals (`1u8`) in the lexer | Ergonomic | New token surface and formatter/AST churn before the type model exists | Lexer/AST/formatter changes |

## Decision

Take option A, with D deferred until the widths exist.

### Type model

- `ae_ty::TyKind::Int(IntTy)` where
  `IntTy = { I8, I16, I32, I64, Isize, U8, U16, U32, U64, Usize }`.
  `ae_ty::TyKind::Float(FloatTy)` where `FloatTy = { F32, F64 }`.
  Add `TyKind::Char` (a Unicode scalar value, not an integer) and
  `TyKind::Unit` (zero-sized, single value).
- `isize`/`usize` are the target pointer width; both supported targets are
  64-bit, so they are 64-bit for now and carry their own name so a 32-bit
  target can change the layout in one place (`ae-layout`, P6.1).
- A migration slice replaces `TyKind::Int` by `TyKind::Int(IntTy::I64)`
  mechanically and keeps behaviour byte-identical before any new width is
  accepted; the same slice introduces `Float(FloatTy::F64)`.

### Literals and inference

- An unsuffixed integer literal types `i64`; an unsuffixed float literal
  types `f64`. AE has no literal suffixes yet (option D), so a non-`i64`
  value is written with an explicit `as` conversion.
- `as` between integer widths is an explicit truncation (two's complement),
  documented as such; it is the programmer's opt-in to narrowing. A literal
  operand of `as` with a constant value outside the target range is a
  compile-time diagnostic, not a silent truncation.
- `as` between integers and floats: int→float rounds to nearest; float→int
  traps unless the truncated value is representable (C's float→int UB is a
  trap here, not UB).
- Integer↔pointer casts exist only inside `unsafe` (preadding ADR-0226
  blocker #2); `usize` is the pointer-sized integer used with them.

### Arithmetic and traps

- Arithmetic operands must have the same type (Hard Rule 1); no implicit
  widening, including between signed and unsigned widths.
- `+`, `-`, `*`, unary `-`, `/`, `%` and shifts trap on overflow, division by
  zero, `MIN / -1`, and a shift count `>=` the bit width. Traps behave
  identically in dev and release (Hard Rule 7) and carry the operation's
  source location, like the existing checked `i64` path.
- `wrapping_*`, `checked_*` (returning `Option[T]`) and `saturating_*` methods
  are the explicit opt-outs; they mirror the runtime helpers so the C backend
  and the interpreter cannot diverge.
- `char` supports `as u32`/`u32 as char` (the latter validates a Unicode
  scalar value) and no arithmetic; `bool` keeps its current rules.
- `()` is the unit type: a function without `->` returns it, `return;` is
  legal in unit functions, and a unit value has no storage or layout.

### Representation

- C backend: `int8_t`…`int64_t`, `uint8_t`…`uint64_t`, `intptr_t`/`uintptr_t`,
  `float`, `double`; `char` is `uint32_t` with the scalar-value invariant;
  unit has no object representation. Checked operations call the runtime's
  width-specific helpers (`__builtin_*_overflow` where available) that abort
  with the existing thread-aware failure path.
- Interpreter: the matching Rust primitives with `checked_*`; the same
  helper names and the same trap text as the C backend.
- MIR: a cast node carries its source and destination type; constants carry
  their scalar type. Bindings already carry `TyKind`, so codegen never has to
  guess a width.

### Migration order

1. ADR + `spec/types.md` semantics; mechanical `Int(I64)`/`Float(F64)`
   migration with the suite byte-identical.
2. `i8`/`u8` end-to-end (lexer literal typing unchanged, `as` casts, checked
   arithmetic helpers, C/interp parity fixtures).
3. Remaining integer widths (`i16`/`i32`/`isize`/`u16`/`u32`/`u64`/`usize`)
   by the same template; boundary fixtures per width.
4. `f32` (literal typing via `as`, math builtins constrained to `f64` or
   widened explicitly), `char` literals/escapes, `()` in signatures.
5. Delete the `I64`-only shims and close P2.1 with the dual-platform gate.

## Evidence

Re-runnable acceptance for the exit gate:

- `cargo test --workspace --locked` on Windows and WSL Ubuntu.
- `python scripts/ab_gate.py --baseline <pre-P2.1>` on both platforms: no
  generated C or diagnostic change for existing programs.
- `python scripts/verify_generated_c.py --sanitize --leak` on WSL.
- New `tests/run` / `tests/ui` boundary fixtures per width and the
  interpreter parity runner (`run --interp` vs compiled output).

## Consequences

- The type model becomes width-complete; `ae-ty` keeps owning the scalar
  domain and the operator rules learn width-aware traps.
- FFI (`repr(C)` structs, extern functions) can express C's fixed-width
  types; the ABI table moves to `ae-layout` in P6.1 without another semantic
  change.
- Existing programs keep compiling unchanged: unsuffixed literals are still
  `i64`/`f64`, and the mechanical migration does not change any output.
- Revisit: literal suffixes (option D) if ergonomics warrant; `i128`/`u128`
  when the backend has a portable 128-bit checked path; 32-bit targets when
  `ae-layout` exists.