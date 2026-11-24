# Generics Specification (M1 subset, current implementation)

Status: describes the implemented language (2026-11-15). Generic monomorphization is the only form: there is no type inference, no runtime generics, and no capability proofs on unsubstituted ASTs — every instantiation is checked as concrete code.

## 1. Design Rules

### Hard Rules:
1. **Explicit Type Arguments**: type arguments are always written (`identity[i64](41)`, `Pair[string, i64]`); the language has no inference, and omitting them is a compile-time error that names the fix.
2. **One Argument List, One Layout**: `Pair[string, i64]` is the same type everywhere — freely assignable, passable, returnable. Different argument lists are different types and never interchangeable.
3. **Check at Instantiation**: a generic body is fully type-checked *after* substitution with the actual arguments; errors point at expressions in the generic body itself. Constraints intercept two earlier points (call-site shape, declaration-site pre-check) but never replace this check.
4. **No Recursive Layouts**: struct and enum names are not registered inside their own bodies, so `struct Node[T] { next: Node[T] }` is rejected — self-referential sizes cannot exist.

### Non-Goals (this version):
- No user-defined traits as constraints: the four built-in constraint names are fixed and not extensible.
- No constraints on generic structs or enums (functions only).
- No `&T` as a return type, struct field, or enum payload.

## 2. Type Parameters

- Written after the name: `struct Pair[K, V]`, `enum Either[T, U]`, `fn identity[T](x: T) -> T`.
- Single uppercase letters (`T`, `U`, `K`, `V`), at most 8 per declaration, no duplicates.
- `main` cannot be generic.

## 3. Generic Structs

`struct Pair[K, V] { left: K, right: V }` — the declaration produces no layout; only an instantiation (`Pair[string, i64] { left: "a", right: 1 }`) is a concrete struct.

- Type arguments may be any nameable type: scalars, `string`, `vec`, `vec[string]`, `map[string, i64]`, structs, enums, or nested instantiations (`Pair[string, Pair[string, string]]`). This is more lenient than generic enums — enum payloads accept only flat types (no mutable layouts), while struct fields naturally hold nested structs.
- Field access, field assignment, move, and drop rules are the concrete ones per instantiation: owned fields are borrowed, never moved out; `p.right = value;` releases the old value first and **owns** the written value afterwards (an owned variable on the right side is consumed); scope exit releases fields according to the instantiated types.
- `pub struct Pair[K, V]` imports like any `pub` item; each program supports at most 256 struct types (regular + all instantiations).

## 4. Generic Enums

`enum Either[T, U] { Left(T), Right(U), Both(T, U), Neither }` — same parameter rules as structs; payloads may be type parameters or fixed flat types.

- Construction names the instantiation: `Option[string]::Some("x")`, `Either[string, i64]::Both("n", 3)`.
- `match` uses the **declaration name** (`Option::Some(v)`); the binding's actual type comes from the matched value (see `spec/patterns.md`).
- The predefined `Option[T] { Some(T), None }` and
  `Result[T, E] { Ok(T), Err(E) }` are public to all files. At most 256 enum
  types per program (built-in + fully instantiated), reported at the
  instantiation site.

## 5. Generic Functions

`fn identity[T](x: T) -> T { return x; }` — the declaration produces no function; each call site (`identity[i64](41)`) instantiates one: parameter types, return type, and body are re-checked after substitution.

- Mismatched argument counts or types error at the call site. Body errors point at the declaration-site expressions (e.g. `'+' requires two strings` inside the generic body).
- The same argument list is instantiated once and cached: `identity[i64]` is the same C function everywhere.
- Type parameters flow into other generics: `fn double_wrap[T](x: T) -> Option[T] { let inner = wrap[T](x); return inner; }` resolves under the current substitution table.
- `pub fn` generics instantiate **across files**: instantiation treats the declaration file as the "current file", so the body may call the declaration file's private functions and types.
- Direct or indirect self-instantiation with the same argument list is rejected at the cycle closure (`is already being instantiated with these type arguments`); recursion with differing arguments is permitted.

## 6. Generic Borrowed Parameters `&T`

`fn size_of[T](value: &T) -> i64` learns `T` at each instantiation site; `&T` maps to the existing read-only borrows with no new lifetimes, borrow kinds, or runtime type information:

| Type Argument | `&T` Instantiates To | Call Site Syntax | Inside the Body |
|---|---|---|---|
| `string` | `string` view | `size_of[string](text)` | `len`, `contains`, `==`, `print`, `+` (copy) |
| `vec` | `&vec` | `size_of[vec](&numbers)` | `len`, indexing, `for x in v`, re-borrowing |
| `vec[string]` | `&vec[string]` | `size_of[vec[string]](&words)` | `len`, indexing (view), `for x in v`, re-borrowing |

- Call-site shape follows the concrete type exactly as non-generic calls do: views passed directly, vector borrows written with `&`. Nothing is moved; the original stays readable and writable after the call.
- Instantiating `&T` with a type that has no read-only borrow (`i64`, `bool`, structs, enums, maps, arrays) reports `has no read-only borrow` at the instantiation site; writing `&T` in a non-generic function reports `unsupported borrow type`.
- `&vec` / `&vec[string]` re-borrow into another read-only parameter (`fn outer(v: &vec) { return inner(&v); }`); mismatched forwarding (`&vec[string]` to `&vec`) is rejected.
- **Known shape limitation**: nesting one `&T` template call inside a generic body varies its call syntax with `T` (`string` passes bare, `vec` needs `&`) — reliable only for the `string` shape today; other shapes separate into concrete functions.

## 7. Constraints

`fn add[T: Add](a: T, b: T) -> T { return a + b; }` — multiple constraints join with `+` (`T: Add + Eq`); different parameters are constrained individually (`fn f[T: Copy, U: Ord](...)`). Constraints exist **only on generic functions**.

Four built-in constraints with fixed names:

| Constraint | Allowed Arguments | Capability |
|---|---|---|
| `Add` | `i64`, `string`, `f64` | `+` (addition or concatenation) |
| `Eq` | `i64`, `bool`, `string`, `f64` | `==`, `!=` |
| `Ord` | `i64`, `f64` | `<`, `<=`, `>`, `>=` |
| `Copy` | `i64`, `bool`, `f64` | Repeatable reads, no drops |

Checked at two levels, **both of which only reject, never permit**:

1. **Call site**: the argument type must be in the constraint's allowlist; the error lists the permitted types. Applies across files for `pub fn`.
2. **Declaration site**: for constrained functions, the **intersection** of all constraint allowlists is the valid set; the body is scanned for operations applied to type parameters, and if a valid type does not support such an operation, the declaration is rejected (`the constraint still admits bool, which does not support it`). Conservative by design: only identifiers immediately adjacent to operation tokens are examined; call results, subscripts, fields, and indeterminate locals are skipped — **false negatives are permitted** (the instantiation check still catches them), **false positives are not**.

Unknown constraints (`T: Printable`), duplicate constraints (`T: Add + Add`), and constraints on structs/enums are rejected at the declaration site. Constraints do not participate in code generation and are not a dispatch mechanism; dispatch through generic bounds via user traits is future work (`spec/methods_and_traits.md`).

## 8. Diagnostics Summary

No dedicated E-codes are assigned in this version; move-related rejections reuse E0102 (`spec/diagnostics.md`). Named messages: omitted type arguments, argument count/type mismatch, `has no read-only borrow`, `unsupported borrow type`, the instantiation-cycle message, the constraint allowlist message (`type argument 'X' for 'Y' does not satisfy 'Z'; that constraint allows ...`), and the declaration-site tightening message.

## 9. Test Matrix

| # | Case | Expected |
|---|---|---|
| G1 | `identity[i64]` / `identity[string]` both called | Two instantiations, correct values |
| G2 | Omitted type arguments | Rejected, names the fix |
| G3 | `Pair[string, i64]` assigned, passed, returned as one type | Single layout, runs |
| G4 | `Pair[string, i64]` mixed with `Pair[i64, i64]` | Rejected (type mismatch) |
| G5 | Owned field write `p.right = value;` consumes an owned variable; scope exit releases fields | Leak-clean, use-after-move rejected |
| G6 | Nested instantiation `Pair[string, Pair[string, string]]` | Runs |
| G7 | Generic enum matched via declaration name for two instantiations | Binding type follows the subject |
| G8 | Generic body error points at the declaration-site expression | Correct location |
| G9 | Cross-file instantiation of a `pub fn` generic that calls a private helper | Runs |
| G10 | Self-instantiation cycle | Rejected at the cycle closure |
| G11 | `&T` instantiated with `string` / `vec` / `vec[string]` | Correct call-site shapes, read-only inside |
| G12 | `&T` instantiated with `i64` | Rejected (`has no read-only borrow`) |
| G13 | `&vec` re-borrowed into another read-only parameter | Runs; pointer passed through |
| G14 | `T: Add` called with `string` and `i64` | Both allowed |
| G15 | `T: Copy` body using `+` | Rejected at the declaration site (tightening) |
| G16 | Unknown / duplicate constraint | Rejected at the declaration site |

See `examples/generic_fn.ae`, `examples/generic_owned.ae`, `examples/generic_borrow.ae`, `examples/generic_impl.ae`, `examples/nested_generics.ae`, `examples/constraints.ae`, `examples/generic_constraints.ae`, `tests/run/generic_impl.ae`, `tests/run/enum_generic_impls.ae`, `tests/run/nested_generics.ae`, and the corresponding `ui` snapshots.
