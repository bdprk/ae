# M2.4.01 — Inventory of Raw Pointer Operations

Status: verified against revision e6610a3 (2026-11-19) on Windows x86-64; the
same source drives both platforms. Scope: every unchecked memory access the
language can emit, plus the runtime entry points that receive pointers and the
safe wrappers whose invariant keeps those pointers valid.

Method: read the tree codegen (crates/ae-compiler/src/frontend/codegen.rs),
the expression parser's pointer arms (expression.rs 459-490, 1180-1215), the
runtime (crates/ae-compiler/src/runtime.c), and grep the emitted C of
tests/run/ptr_offset.ae and tests/run/ptr_retag.ae.

## 1. Raw operations: what safe and unsafe code can ask for

| Operation | Spelling | AST/MIR | Generated C | Check before emission |
|---|---|---|---|---|
| Borrow to pointer | `&v as *const T`, `&mut v as *mut T` | `Expr::BorrowAs` | `&(v)` plus a C cast | safe; the operand must be a variable whose type is not owned (expression.rs rejects a pointer to an owned value, so safe code cannot dangle one) |
| Reinterpret cast | `p as *const U` | `Expr::PtrCast` | `(U *)(p)` | safe; address identity only, no runtime step |
| Raw read | `p.deref()` | `Expr::DerefPtr` | `AE_PTR_READ(p)` | `unsafe` required; state checked by the caller |
| Raw write (expression) | `ptr_write(p, v)` | `Expr::PtrWrite` | `AE_PTR_WRITE(p, v)` | `unsafe` required; `*mut T` only |
| Raw write (statement) | `*p = v;` | `Statement::PtrStore` | `AE_PTR_WRITE(p, v)` | `unsafe` required |
| Pointer arithmetic | `p.offset(n)`, `p.byte_offset(n)` | `Expr::PtrOffset` | `(T *)(p + n)` / `(T *)((char *)p + n)` | `unsafe` required; no bounds or alignment reasoning |
| In-place drop | `ptr_drop_in_place(p)` | `Expr::PtrDropInPlace` | `ae_*_release((T *)p)` | `unsafe` required; `*mut T` only |
| Layout query | `c_size_of[T]()`, `c_align_of[T]()`, `c_offset_of[T](f)` | constant | literal | compile time; `#[repr(C)]` layout only |

Every unchecked read and write is emitted as `AE_PTR_READ` /
`AE_PTR_WRITE` (runtime.c, M2.4.03). `grep -n AE_PTR_ <generated>.c`
lists every promise a program makes about memory, and the two macros are the
only place the contract needs reviewing.

## 2. Runtime entry points that take pointers

| Function | Receives | Assumes | Who releases |
|---|---|---|---|
| `ae_str_release(ae_string *slot)` | pointer to an owned string slot | the slot is live and not aliased; the pointer is either NULL, read-only data, or an allocator block | call itself: frees, then nulls the slot |
| `ae_gvec_release`, `ae_strvec_release`, `ae_map_release` | pointer to the container slot | same as above | call itself |
| `ae_t<N>_release`, `ae_e<N>_release` | pointer to a struct or enum value | the value is initialized and owned by the caller | call itself, field by field |
| `ae_gvec_push(ae_gvec *v, const void *elem, ...)`, `ae_strvec_push`, `ae_map_put(ae_map *m, ...)` | pointer to a mutable slot | exclusive access; no live view of the same buffer | the slot keeps ownership |
| `ae_slice_view(s, start, end)` | view by value, returns a view into `s.ptr` | `s.ptr` stays live as long as the result (enforced by the borrow checker, not by C) | nobody: views are never freed |
| `ae_str_slice(s, start, end)` | view by value | returns an owned copy | the returned string |

## 3. Safe wrappers and the check that enforces their invariant (M2.4.04)

| Wrapper | Length | Alignment | Lifetime | Where enforced |
|---|---|---|---|---|
| `slice_view(s, a, b)` | `a < 0`, `b < a`, `b > s.len` trap; UTF-8 boundary check | not applicable (byte view) | the view borrows `s`; NLL keeps `s` from moving or being written while the view is read | runtime.c `ae_slice_view` + borrow checker |
| `tokenize(s, sep)` cursor | token framing checks inside the cursor; separator is copied | not applicable | the cursor holds a loan on `s` | runtime.c cursor + borrow checker |
| `&mut self` method call | type-checked receiver | inherits the receiver's alignment | exclusive loan for the call's extent | borrow checker (ADR-0185/0205) |
| `&v as *const T` | one element, no length | `&v` is naturally aligned for `T` | untracked by design; the operand must not be owned | expression.rs `ty.owned()` rejection |
| `c_size_of` / `c_align_of` / `c_offset_of` | static | `#[repr(C)]` field alignment | not applicable | parser constant folding; spec/unsafe.md 5.1 |
| `extern "C"` pointer parameters | the C side's | the C side's | the C side's | protocol: doc comment on the declaration (E0704) |

A raw pointer obtained from `&v` therefore inherits extent 1 and `T`'s natural
alignment; only `offset` / `byte_offset` and C-supplied pointers can break
those two, and both require `unsafe`.

## 4. Recorded gaps

- spec/unsafe.md 2.2 names `p.write(value)`, `p.read()`, `ptr::null[T]()` and
  `p.is_null()`. None is implemented; the implemented spellings are
  `ptr_write(p, v)`, `*p = v`, `p.deref()`, `p.offset(n)`, `p.byte_offset(n)`
  and `ptr_drop_in_place(p)`. A spec edit should either drop the four names or
  list them as future work.
- Alignment is never checked at runtime. It cannot be violated through safe
  code (see section 3); `unsafe` offset arithmetic and C pointers can break it,
  and the sanitizer corpus (M2.4.05) is the only backstop.
- The interpreter models raw reads and writes in a per-slot table, so a
  same-typed store followed by a read is exact while cross-type punning is
  documented as limited (ADR-0209 step A).
