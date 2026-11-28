# ADR-0245: Owned pointees through heap pointers

- Status: Proposed
- Date: 2026-11-28
- Associated Milestone: P4.1 (real `std/alloc` containers), spec/unsafe.md 2.1-2.4

## Context

`Box[T] { ptr: *mut T }` landed with generic pointer fields (slice 80), but
the owned-pointee surface around it is gated in four places:

1. the layout queries name scalars, enums and `#[repr(C)]` structs only —
   `c_size_of[string]` and `c_size_of[T]` with `T = string` are rejected
   (`expression.rs`), so a generic container cannot size a `string` slot;
2. the retag (`p as *mut T`) and `ptr::null[T]()` pointee chains admit
   scalars, substituted scalars and repr(C) structs — `string` is excluded
   (`lower_expr.rs`) with the note "a tracked layout stays out — its bytes
   cannot gain a second owner";
3. `p.deref()` types as the bare pointee (`mir.rs` `expr_type`); for an
   owned pointee that is a value read, and binding it would copy the string
   header (two owners, one allocation);
4. `ptr_write` already transfers ownership into the slot and
   `ptr_drop_in_place` already emits the full release chain for every owned
   pointee family in both C backends (`codegen.rs`) — the language pieces
   past the gates are done.

The two C runtimes also disagree on the byte layout of a string today: the
legacy `ae_string` is `{char *, int64_t}` (16 bytes) while the pure
runtime's `ae_str` is `{const char *, int64_t, bool owned}` (24 bytes). A
layout query with one answer needs one table; the pure layout is the ABI
target (ADR-0242 retires the legacy backend in favor of the pure one).

## Decision

Staged, each stage behind the existing gates:

1. **Layout + admissions (this slice).** `c_size_of[string]`/`c_align_of[string]`
   pin **24 / 8** — the pure runtime's `ae_str` — and the substituted form
   `c_size_of[T]()` resolves through `T → string` the same way. The retag
   and `ptr::null[string]()` admit the `string` pointee. The legacy runtime
   over-allocates a 16-byte string in the 24-byte slot until its retirement;
   its release reads only the first 16 bytes, so the shape stays sound.
   The slot-level ownership contract is exactly spec/unsafe.md 2.4's:
   the author of the `unsafe` block owes a fresh allocation (or a slot
   whose ownership they hold), one `ptr_write`, and one release.
2. **Drop-through containers.** `box_free[T]` releases the pointee in place
   (`ptr_drop_in_place`) before freeing the slot, so any `T` — scalar,
   struct, or owned — frees completely; `box_get`/`box_take` keep their
   current value rules until stage 2.
3. **Deref is a view (next slice).** `p.deref()` on an owned pointee types
   as the view (`StrView` for `string`), emits a view construction on both
   backends (`AE_VIEW` legacy, an `owned=false` copy pure), and the
   interpreter returns a view value — no header copy ever gains a second
   owner. `box_get` on an owned payload returns the view then, and moving
   a value out of a heap slot stays rejected (the field rules govern).

## Consequences

- Generic containers can size and release owned slots now; `Box[string]`
  allocation and `box_free` work end to end, verified leak-clean.
- The layout constant is one number on both backends; the legacy runtime's
  over-allocation is a documented, temporary shape that its retirement
  deletes.
- The interp's `ptr_drop_in_place` stays a named trap (its slot model has
  no heap block to release); the compiled backends carry the corpus
  evidence, per the established interpreter-trap convention.
