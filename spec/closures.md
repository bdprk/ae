# Closures and Function Types Specification (M1.8, ADR-0188 / ADR-0189)

Status: describes the implemented language (2026-11-24, P2.3). Direct local closures compile to ordinary calls; storable `fn(T1, T2) -> R` function pointers, named safe function references, `Fn`/`FnMut`/`FnOnce` trait bounds, anonymous closure environment structs, and `-> impl Fn*` returnable owning closures are supported.

## 1. Design Rules

### Hard Rules:
1. **Zero-Cost Direct Shape**: a local closure called directly compiles to an ordinary function whose leading parameters are its captures, spliced in at the call site.
2. **Storable Function Pointers (`fn(T1, ...) -> R`)**: `fn(...) -> R` (with omitted `->` defaulting to `()`) is a `Copy` value type usable in locals, parameters, return types, struct fields, enum payloads, and generic containers (`Option[fn(...)]`). Safe non-generic top-level functions and non-capturing closure literals coerce to `fn(...) -> R`; `unsafe fn` and capturing closures are rejected.
3. **First-Class Capturing Closures (`Fn` / `FnMut` / `FnOnce`)**: when a closure is passed to a generic `F: Fn(...) -> R` / `FnMut(...) -> R` / `FnOnce(...) -> R` bound, stored in a generic struct (`struct Runner[F] { cb: F }`), or returned via `-> impl Fn(...) -> R` / `impl FnMut(...) -> R` / `impl FnOnce(...) -> R`, it materializes as an anonymous closure environment struct (`__ae_closure_env_<id>`) holding its captured values.
4. **No Escaping Frame Borrows**: returning a closure via `-> impl Fn*` that borrows a local variable from the returning frame is rejected (`cannot move a value while it is borrowed`); `-> impl Fn*` closures own their captured values.

## 2. Closure Literals

### 2.1 Syntax
```
let add = |a: i64, b: i64| -> i64 { return a + b; };
let base = 10;
let offset = |x: i64| -> i64 { return base + x; };   // `base` captured
let tick = || -> i64 { return 1; };                  // `||` is the empty parameter list
```
- Parameter and return annotations are **required**; there is no inference for parameter types anywhere else in the language, and the return type is what makes call sites checkable.
- The body must return on every path, like any function, and may not return a borrow.
- `||` disambiguates in the parser: `a || b` remains logical-or because an or-expression is never followed by `->`; `Or Arrow` can only be a parameterless closure. One token of context, no lexer change.
- Closures nest: an inner body may capture from any enclosing scope.

### 2.2 Capture Inference (the Fn/FnOnce split)
The literal parses its body twice; both probes are throwaway, only the last parse is emitted:

1. **Borrow probe** — every borrowable capture is bound in its read-only form (`string` → view, `vec` → `&vec`, `struct` → `&Struct`) and the rest as-is. If the body parses, every capture is a borrow: the closure is callable any number of times, and the outer variable cannot move or be written while the closure name is live (`cannot assign to 'greeting' while it is borrowed`).
2. **Owned probe** — if the borrow probe fails (some capture is used where an owned value is required, e.g. `return note;` hands the captured string's ownership to the call), everything is bound by value and every used capture becomes a **value capture**: each call moves the outer variable, exactly like a plain call. A second call is a compile-time use-after-move (`cannot call 'f' again because a value capture was already moved by the first call`, E0102), not a runtime double free.

An AST walk over the surviving probe decides *which* names are captures (used parameter slots, in capture order) and rejects the one shape that cannot be honored: mutating a captured container or field (`a closure cannot mutate captured variable 'items' in this version`).

### 2.3 Capture Modes by Type
- Scalars (`i64`, `bool`, `f64`): always copy — no borrow form, no move; reusable across calls.
- `string`, `vec`, `vec[string]`, `vec[f64]`, user structs: borrow (read-only view) if the body allows, otherwise by value.
- Maps: travel by value (maps cannot be borrowed anywhere in the language) — one call consumes them.
- Capturing a variable that was already moved is rejected at the literal (`cannot capture 'note' because it was moved`, E0102).

### 2.4 Loans
- Every borrow capture registers a persistent borrow on the outer variable — the same register a string view uses: the variable cannot be moved, written, or reassigned while the closure name is live. The loan ends at scope exit.
- Borrows taken *inside* the body end with each call: the callee owns nothing the caller depends on.
- Value captures move at each call; the outer variable is unusable after the first call of a value-capturing closure (re-initialization by assignment is allowed, as with any moved variable).

## 3. Function Types (`fn(T) -> R`, ADR-0189)

### 3.1 Syntax
```
fn apply(f: fn(i64) -> i64, x: i64) -> i64 {
    return f(x);
}
let doubled = |v: i64| -> i64 { return v * 2; };
print(apply(doubled, 5));   // argument is the closure literal's name
```
- The argument must be a **closure literal** (bound to a name first) whose shape matches exactly and that captures nothing — a capturing closure has no representation without its environment; bind its inputs as arguments instead.
- Identical signatures intern to one type, so two closures of the same shape are interchangeable.
- Inside the callee, `f(x)` is a call through the pointer the caller passed; there is no dynamic dispatch surface and no indirect-call checks beyond the ordinary.
- Function values cannot be returned, stored in fields, or applied to names that are not closure literals.

### 3.2 Diagnostics
- Passing a capturing closure where `fn(T) -> R` is expected: rejected — a capture has no representation across the boundary.
- `apply(double, 5)` (a bare named function): rejected — named-function references do not exist in this version.
- Shape mismatch (parameter count or types, return type): rejected at the call site as a type error.

## 4. Diagnostics Summary

| Message (stable text) | Code | Situation |
|---|---|---|
| `cannot capture '<name>' because it was moved` | E0102 | The literal names a moved variable |
| `cannot call '<name>' again because a value capture was already moved by the first call` | E0102 | Second call of a value-capturing closure |
| `a closure cannot mutate captured variable '<name>' in this version` | — | Body writes through a capture |
| `a closure cannot be returned: it has no storage, and its captures live in this frame` | — | `return` of a closure |
| `cannot assign to '<name>' while it is borrowed` | — | Write to a borrow-captured variable while the closure is live |

Codes are assigned only where classified; the human-readable message is not a stable identifier (see `spec/diagnostics.md`).

## 5. Test Matrix

| # | Case | Expected |
|---|---|---|
| C1 | Two-parameter closure called twice; scalars copy | Both calls return correct values |
| C2 | Borrow capture of a `string`/struct, called twice; outer variable read after | Runs; outer variable intact |
| C3 | Write to the outer variable while a borrow-capturing closure is live | Rejected (loan diagnostic) |
| C4 | Value capture (`return note + "!"`), first call, then second call | Second call rejected E0102 at the call site |
| C5 | Body pushes into a captured `vec` | Rejected (mutation diagnostic names the enclosing scope) |
| C6 | `return` a closure from a function | Rejected (no-storage diagnostic) |
| C7 | Capture a moved variable | Rejected E0102 at the literal |
| C8 | Nested closures: the inner body captures from the middle scope | Runs |
| C9 | Non-capturing closure passed to a `fn(i64) -> i64` parameter, called in the callee | Runs; call is through the passed pointer |
| C10 | Capturing closure passed to a `fn(T) -> R` parameter | Rejected |
| C11 | Two closures with identical signatures used for one parameter | Interchangeable — both accepted |
| C12 | `a \|\| b` used as a value (not followed by `->`) | Parsed as logical-or, not a closure |

See `tests/run/closures.ae`, `examples/closures.ae`, `examples/function_types.ae`, and the `ui` snapshots (`closure_*`).
