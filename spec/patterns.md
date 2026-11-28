# Enums and Pattern Matching Specification (M1.9 subset, current implementation)

Status: describes the implemented language (2026-11-26). Wildcard `_` arms/payloads, nested enum destructuring, exhaustiveness/reachability/redundancy checking, and non-Copy payload borrow protection are enforced today; guards, `@` bindings, ranges, `if let`, `let else`, and `while let` remain future work.

## 1. Design Rules

### Hard Rules:
1. **Exhaustiveness Is Not Optional**: a `match` must cover every variant (and nested variant combination) of the matched enum. Wildcard `_` is supported as a catch-all arm (`_ => { ... }`) or in payload positions (`Color::Custom(r, _)`), and every arm is checked for exhaustiveness, reachability, and redundancy: missing combinations, arms shadowed by an earlier `_` or nested wildcard, and redundant `_` arms when all cases are already covered are compile-time rejections.
2. **Bindings Borrow, Never Move**: a pattern binding is a read-only binding (including directly-bound non-Copy enum payloads such as `Outer::Wrap(inner)`); payloads cannot be moved out or returned from the arm. Ownership of the whole enum is the only ownership there is.
3. **The Subject Is Borrowed for the Match**: while the `match` runs, the subject variable cannot be moved or reassigned as a whole. Rebinding other variables is unaffected.

### Non-Goals (this version):
- No guards (`if` on a binding), no `@` bindings, no binding ranges, no `if let` / `let else` / `while let`.
- `match` is a **statement**, not an expression: it produces no value, and its subject must be a bare enum variable (no expression subjects).

## 2. Enum Declarations

```
enum Shape { Circle(i64), Rect(i64, i64), Empty }
enum Either[T, U] { Left(T), Right(U), Both(T, U), Neither }
```
- Variants carry 0–16 positional payloads; non-generic payload types are limited to `i64`, `bool`, `string`, `vec`, and `vec[string]`. Generic enums may also carry their type parameters. Enums cannot be self-contained — recursive layouts do not exist.
- Variant names are unique within an enum; enum names conflict with structs, other enums, and built-ins like any top-level name.
- Enums are owned values: assignment, pass-by-value, and `return` move the entire enum; use after move is rejected at compile time; scope exit, early returns, and jump paths release payloads according to the *current variant*; assigning a new value releases the old payload first. `string` payloads are neither leaked nor double-freed.
- Enums cannot be `print`ed whole or compared as a whole (`==`/`!=` are rejected on enum-typed operands).

## 3. Construction

```
Shape::Circle(3)          // positional payloads, count and types must match
Option[string]::Some("x") // generic instantiation names its type arguments
Option[i64]::None
```
- The number of payloads must match the declaration; `string` payloads are passed by move.
- Generic enums require explicit type arguments; one argument list produces one layout (`Option[string]` is the same type everywhere), different arguments are different types (`Option[string]` and `Option[i64]` are not interchangeable).
- `::` is shallow on an enum: the name after it decides — a variant name is a variant path; anything else reaches the enum's impl associated functions (ADR-0186).

## 4. `match`

```
match at {
    IntResult::Ok(n) => { print(n); }
    IntResult::Err(reason) => { print(reason); }
}
```
- The subject must be a **bare enum variable**. Every variant (and nested variant combination) must be covered: missing variants/combinations (`match is not exhaustive: 'Outer::Wrap(Inner::B)' is not handled`), duplicate arms, wrong-enum variants, unreachable arms after `_` or an earlier nested wildcard (`unreachable match arm after wildcard '_'`), and redundant `_` when all cases are already covered (`wildcard '_' is redundant: all variants are already covered`) are rejected.
- **Wildcards and nested patterns (P2.5.3)**: `_` matches any remaining variant (`_ => { ... }`) or ignores an individual payload (`Color::Custom(r, _)`). Enum payloads can be destructured recursively (`Outer::Wrap(Inner::Val(n))`, `Holder::Full(Option::Some(v))`).
- **Bindings are read-only**: `i64`/`bool`/`f64` payloads are copied by value; `string` payloads are bound as `string` views; `vec` as `&vec`; `vec[string]` as `&vec[string]`. Directly-bound non-Copy enum payloads (`Outer::Wrap(inner)`) are borrowed for the arm and cannot be moved into a call or returned (`while it is borrowed`), though they can be `match`ed on inside the arm. None can be moved out of the enum.
- Views bound this way follow the view rules (`len`/`contains`/`slice`/`==`/`print`/`+`); borrowed vectors support `len`/indexing/`for-in`/further borrowing.
- The subject is borrowed for the duration of the `match`: it cannot be moved or reassigned as a whole inside the arms.
- `break`/`continue` inside an arm apply to the innermost *loop*, not the `match`.
- Generic enums match by the **declaration name** (`Option::Some(v)`), not the instantiation name; the binding's actual type comes from the matched value.

## 5. Interaction with `?` Propagation

Propagation requires one of two variant shapes, with fixed variant names and order:
- `{ Ok(...), Err(string) }` — all nine predefined result enums satisfy this; a custom enum with the same shape is also propagatable.
- `{ Some(T), None }` — the predefined `Option[T]` and custom enums of the same shape.

- `let x = call()?;` binds the success payload (success variants must have exactly one payload); `call()?;` as a bare statement releases the payload and continues — it is the language's only expression-statement form and **must** end with `?`.
- On the failure path, all still-alive owned variables in the current function are released first, then the result value is returned to the caller.
- Propagation requires the enclosing function to return the **same** enum type; one function propagates one family only (crossing `DirResult` and `StatusResult` needs a `match` at the boundary); `?` cannot be used in `main`.
- `?` is a statement, not an expression — it cannot appear inside a larger expression.

## 6. Diagnostics Summary

Exhaustiveness, duplicate arms, unknown variants, wrong subject shape, whole-enum `print`/comparison, moved-subject use, and payload move-out attempts are compile-time rejections; the human-readable messages are not stable identifiers and no dedicated E-codes are assigned in this version (move-related rejections reuse E0102, see `spec/diagnostics.md`).

## 7. Test Matrix

| # | Case | Expected |
|---|---|---|
| P1 | Exhaustive match over a three-variant enum, one binding per payload kind | Runs; scalar copies, string view |
| P2 | Match missing one variant | Rejected (exhaustiveness) |
| P3 | Match with a duplicated variant | Rejected (duplicate arm) |
| P4 | Match naming a variant of a different enum | Rejected |
| P5 | Moving a `string` payload binding out of the arm | Rejected (bindings are views) |
| P6 | Moving / reassigning the subject inside an arm | Rejected (subject borrowed) |
| P7 | `break` inside a `match` inside a loop | Exits the loop, not the `match` |
| P8 | Generic enum matched via `Option::Some(v)` for both `Option[string]` and `Option[i64]` | Both compile; binding type follows the subject |
| P9 | Custom enum shaped `{ Ok(T), Err(string) }` used with `?` | Propagates like a predefined result |
| P10 | `?` crossing result families in one function | Rejected (one family per function) |
| P11 | Enum used after being moved into a call | Rejected E0102 |
| P12 | Scope exit / early return releases the current variant's payloads (leak check) | `--leak` clean |
| P13 | Wildcard `_` arm and payload `_` across all 4 backends | Runs; catch-all and ignored payloads behave identically |
| P14 | Nested enum destructuring (concrete and generic, e.g. `Holder::Full(Option::Some(v))`) | Runs across all 4 backends |
| P15 | Missing nested sub-variant, arm after `_`, or redundant `_` | Rejected (exhaustiveness / unreachable / redundant) |
| P16 | Directly-bound non-Copy enum payload (`Outer::Wrap(inner)`) moved/returned vs matched | Move/return rejected (`while it is borrowed`); nested `match inner` runs |

See `tests/run/enum_methods.ae`, `tests/run/enum_generic_impls.ae`, `tests/run/pattern_matching_p25.ae`, `tests/pattern_matching_p25.rs`, `examples/enums.ae`, `examples/enum_methods.ae`, `examples/option_propagate.ae`, and the corresponding `ui` snapshots (`tests/ui/pattern_after_wildcard.ae`, `tests/ui/pattern_nested_non_exhaustive.ae`, `tests/ui/pattern_wildcard_redundant.ae`).
