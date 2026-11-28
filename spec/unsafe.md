# Unsafe, Raw Pointers, and FFI Specification (M2.12)

Status: target specification for the M2.12 milestone. The current language (2026-11-14) implements none of this; the spec lands before the implementation per the plan's Spec-First rule, and each section gains a positive/negative test pair as its slice merges.

## 1. Design Rules

### Hard Rules:
1. **Unsafe Never Weakens Safe Code**: every guarantee safe code enjoys (ownership, borrow checking, bounds, checked arithmetic, initialization) holds unchanged. `unsafe` only unlocks operations the checker cannot prove; it never disables a check on code the checker can prove.
2. **The Author Owns the Invariant**: every `unsafe fn` documents its preconditions in a doc comment; every `unsafe {}` block carries a comment naming the invariant that makes it sound. A violation is undefined behavior — the language's safety story (ADR-0205) does not extend across this boundary.
3. **No Safe Path to UB**: constructing a dangling pointer, an unaligned reference, or an out-of-bounds view must require `unsafe` (or an `unsafe fn`) somewhere in the program. If a safe spelling reaches UB, that is a compiler bug with the same severity as a borrow-checker false negative.
4. **Explicitness at Every Boundary**: raw pointers do not convert to references implicitly; `extern "C"` functions are declared, not inferred; `#[repr(C)]` is opt-in per struct. Nothing about the C boundary is ambient.
5. **Same Semantics on Both Platforms**: an FFI program's observable behavior (exit codes, stdout, file effects) is identical on Windows and Linux; the C backend's generated code already satisfies this for pure code.

### Non-Goals:
- No safe transmute of arbitrary values; no reinterpretation without `unsafe`.
- No lifetime-carrying raw pointers: a raw pointer is just an address, typed by pointee layout only.
- No C++-style inheritance or exception interop: `extern "C"` is a flat C ABI.

## 2. Raw Pointers

### 2.1 Types
- `*const T` and `*mut T` for any `T` with a known layout (scalars, `#[repr(C)]` structs, arrays, `string` — whose layout constant is the pure runtime's 24-byte `ae_str`, ADR-0245).
- Copyable, comparable for equality, passable by value; they carry no ownership and are not tracked by the borrow checker. Creating one from a reference is safe; using one to read or write memory is `unsafe`. A heap slot (`ae_std_alloc`) may hold an owned value: `ptr_write` transfers ownership to the slot and `ptr_drop_in_place` releases it — the author of the `unsafe` block owes spec 2.4's slot contract.

### 2.2 Operations (all require `unsafe`)
- `p.deref()` → `T` for `Copy`-sized `T`; for `*mut string` the read is a **view** (`StrView`): print and comparison work on it and it releases nothing; binding it to a variable is rejected until heap views are storable (ADR-0245).
- `p.write(value)` / `p.read()` — not implemented; the implemented spellings are `ptr_write(p, v)` (the ownership-transfer form) and `*p = v` (owned pointees rejected: the pointer would take over their release).
- `p.offset(n)`, `p.byte_offset(n)` — pointer arithmetic, no bounds reasoning.
- `ptr::null[T]()`, `p.is_null()` — implemented; `is_null` is safe (it reads nothing).
- `&v as *const T` / `&mut v as *mut T` — borrows to pointers, safe (an explicit cast); the operand must not be owned.
- `p as usize` / `usize as *const T` — integer round trips, safe casts; what you do with the result is not.

### 2.3 Diagnostics
- Dereferencing outside `unsafe` is rejected: `dereferencing a raw pointer requires 'unsafe'` (E0700).
- A raw pointer escapes to a non-pointer type only through an explicit cast; no inference bridges.

### 2.4 Alignment and Provenance of One Typed Read

A read `p.deref()` — and the writes `ptr_write(p, v)` / `*p = v` — is defined only when all four hold:

1. **Origin.** `p` came from `&v as *const T` / `&mut v as *mut T` on a live binding of type `T`, from a C pointer received through `extern "C"`, or from `offset` / `byte_offset` / an explicit cast applied to such a pointer.
2. **Extent.** The address and the `c_size_of[T]()` bytes after it lie inside the same allocation, or the same live local, that the origin named. `offset(n)` and `byte_offset(n)` are unchecked: keeping the result inside that extent is the caller's obligation.
3. **Alignment.** The address is a multiple of `c_align_of[T]()` — 8 for `i64`, `f64` and pointers, 1 for `bool`, the maximum field alignment for a `#[repr(C)]` struct. Taking `&v` supplies it; only arithmetic and C-supplied pointers can break it.
4. **Initialization and aliasing.** The bytes hold a value of `T` written earlier, and no tracked borrow that overlaps them is live at the access.

Breaking any of the four is undefined behavior (hard rule 3). Safe code cannot reach a violation: `&owned as *const T` is rejected, a pointer to a moved value is not constructible from safe spellings, and the interpreter's per-slot model traps a same-typed read of a moved or unwritten slot. Nothing checks alignment at runtime, so an `unsafe` block that computes an address owes rule 3 itself.

## 3. `unsafe` Blocks and Functions

### 3.1 Syntax
```
unsafe { ... }                       // block: the operations inside are unchecked
unsafe fn poll(fd: i64) -> i64 { ... }   // the whole body is unchecked territory
```

### 3.2 Rules
- A body may call `unsafe fn`s and dereference raw pointers inside an `unsafe` block only.
- `unsafe fn` bodies do not implicitly behave like `unsafe` blocks — the body still requires `unsafe {}` internally (Rust 2024 style): the reviewer sees where unchecked memory access actually happens.
- Calling an `unsafe fn` from safe code requires an enclosing `unsafe` block.
- `unsafe` propagates no capability through closures: a closure literal's body is checked like any other safe body, and captures may not lend `*mut` storage the closure writes without `unsafe`.
- The borrow checker keeps checking inside `unsafe` blocks: creating a reference from a pointer inside `unsafe` re-enters the tracked world, and the usual rules apply from that point.

### 3.3 Diagnostics
- `call to unsafe function 'poll' requires 'unsafe'` (E0701) at the call site.
- An `unsafe` block may not be silent decoration: a block whose body contains no unchecked operation triggers the `unnecessary unsafe` warning (W0702).

## 4. `extern "C"`

### 4.1 Declarations (calling C)
```
extern "C" fn getenv(name: *const u8) -> *const u8;      // no body: resolved at link time
extern "C" fn abs(v: i64) -> i64;
```
- A declaration without a body is emitted as a C declaration (`extern` prototype); the generated program links against the symbol.
- Argument and return types must be C-ABI-shaped: scalars, `*const T`/`*mut T`, `#[repr(C)]` structs by value, and `&[i64; N]`-style borrowed arrays are legal; owned strings, vectors, maps, and enums are not (they carry release logic the C side cannot honor).
- Calling a declared `extern "C"` fn is `unsafe` — the C side's preconditions are unknown to the compiler.

### 4.2 Exposures (C calling ae)
```
#[no_mangle]
extern "C" fn ae_add(a: i64, b: i64) -> i64 { return a + b; }
```
- `#[no_mangle]` keeps the symbol name; without it the name is not emitted as a C-visible symbol.
- An exposure's body is safe code: the compiler wraps it, so the C side cannot observe an ae panic directly — a trap inside an exposure aborts the process with the runtime's trap diagnostics (same as today).
- Exposures with `&Struct`/`&mut Struct` parameters or `#[repr(C)]` by-value structs are the only structured crossing points.

## 5. `#[repr(C)]`

### 5.1 Layout
- `#[repr(C)] struct Pair { a: i64, b: i64 }` fixes field order and C padding rules (each field at its natural alignment, struct alignment = max field alignment).
- Without the attribute, layout is the compiler's choice (today: declaration order, natural alignment); `#[repr(C)]` is required for any struct crossing the FFI boundary in either direction.
- A `#[repr(C)]` struct may contain another concrete `#[repr(C)]` struct. A non-`#[repr(C)]` struct field or a field owning storage is rejected; this exception does not make arbitrary structs C-compatible.
- A `#[repr(C)]` struct must contain at least one field. Empty structs are not a portable C11 type and cannot establish an FFI layout contract.
- The layout is pinned by a compile-time query pair, `c_size_of[T]()` and `c_offset_of[T]("field")` (each field's byte offset), evaluated in the parser to constants. The `c_` prefix keeps the pair distinct from the generic-borrow convention's user-definable `size_of[T]`; the names belong to the same `#[repr(C)]`/`extern "C"` family.

#### Scalar size query

`c_size_of[T]()` and `c_align_of[T]()` accept `i64`, `f64`, `bool`, and `#[repr(C)]` structs, including when `T` is a generic parameter substituted with one of these types. Both values for the three scalars in the current x86-64 Windows and Linux C ABI model are respectively 8, 8, and 1 bytes, matching the existing struct-layout calculation. Struct alignment is the maximum field alignment. `c_offset_of` remains limited to `#[repr(C)]` structs; scalars have no fields. Unknown types, owned types, and non-`#[repr(C)]` structs remain rejected. These queries do not establish allocator compatibility or container soundness; other target ABIs require a separate verified layout model.

### 5.2 Diagnostics
- Passing a non-`#[repr(C)]` struct across `extern "C"` is rejected: `'Pair' is not '#[repr(C)]'; the C ABI requires a fixed layout` (E0703).

## 6. Precondition Documentation

Every `unsafe fn` and every nontrivial `unsafe {}` block must carry a doc comment naming:
1. What memory the caller must guarantee valid (pointers non-null, in-bounds, aligned).
2. What aliasing the caller must guarantee absent (no live borrows overlapping the raw pointer's access).
3. What the callee assumes about thread context (single-threaded, or which synchronization the caller owes).

The compiler enforces presence (a doc comment is required on `unsafe fn`, E0704); it cannot enforce truth.

## 7. Test Matrix (each cell lands with its slice)

| # | Case | Expected |
|---|---|---|
| U1 | `let p = &v as *const i64; unsafe { print(p.deref()); }` | Runs, prints the value |
| U2 | `let p = &v as *const i64; print(p.deref());` | Rejected E0700, positioned at the deref |
| U3 | `unsafe fn f() { print(1); }  f();` (no unsafe at the call) | Rejected E0701 |
| U4 | `unsafe { f(); }` where `f` is safe | W0702 |
| U5 | `extern "C" fn abs(v: i64) -> i64;` called inside `unsafe` | Links against libc `abs`, runs |
| U6 | An `extern "C"` declaration taking a `string` | Rejected (owned types are not C-ABI-shaped) |
| U7 | `extern "C" fn` exposed with `#[no_mangle]`, called from a C stub | Bidirectional example runs on both platforms |
| U8 | A `#[repr(C)]` struct passed by value to a declaration | Matches the C layout check (`c_size_of`/`c_offset_of`); the by-value call compiles with a real C prototype |
| U9 | A struct without `#[repr(C)]` crossing the boundary | Rejected E0703 |
| U10 | `unsafe fn` without a doc comment | Rejected E0704 |
| U11 | A dangling deref under the MIR UB-detection mode (M2.11) | Trapped with a pointer diagnostic. Today the shape is prevented at the declaration — a pointer to an owned value is rejected because its borrow is untracked — so the interpreter's trap ("reading through a pointer to a moved value") is the backstop that arms when pointer-to-owned lands |
| U12 | Differential: every U-case's safe mirror compiles and runs identically under interp / MIR / tree / release | Four-way agreement |
| U13 | The allocator bridge: `extern "C" fn ae_std_alloc(...) -> *mut i64` links against the runtime, allocates, and frees through the same accounting every built-in uses (ADR-0208) | `tests/run/extern_alloc_ok.ae` runs; `extern_alloc_oom.ae` terminates with the caller-passed position |
| U14 | `offset(n)` / `byte_offset(n)` move the address inside `unsafe`; both keep the base's pointer type, and outside `unsafe` are rejected (ADR-0209) | `tests/run/ptr_offset.ae` prints 10/20/30 through three views of one buffer |
| U15 | `c_size_of[T]()` / `c_offset_of[T](field)` resolve a type parameter through the instantiation's substitution table (ADR-0209) | `tests/run/generic_layout_query.ae`: generic body 16 == concrete call 16 |
| U16 | Pointer type annotations are writable in `extern "C"` signatures (`-> *mut i64`), completing §4.1's pointer forms | Slice-1 corpus compiles; a non-pointer `as` still names the value-conversion path |
