# Changelog

All notable, user-visible changes to the `ae` toolchain and the language it compiles are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project has not cut a numbered release yet — everything below lives under **Unreleased** until the first tagged version. Internal work (test frameworks, ADRs, CI plumbing, the differential fuzzer) is listed only where a user can observe it.

## [Unreleased]

### Added — language

- **Heap containers through raw slots** (ADR-0245). A `#[repr(C)]`-layout constant for owned values (`c_size_of[string]()` = 24, `c_size_of[vec[i64]]()` / `c_size_of[map[string, i64]]()` = 40) lets an `ae_std_alloc` slot hold any of them; `ptr_write` transfers ownership into the slot, `ptr_drop_in_place` releases it, and the block frees — the raw-pointer form of `Box[T]` / `Vec[T]` / `Map[T]` allocation. Reading through a slot pointer is a **view** (`StrView` for strings, `VectorRef` for vectors): print, comparison, indexing, `len`, and `for` iteration consume the view without copying the owning header, on all three execution routes byte-identically.
- **`Rc[T]` shared ownership** (`std/alloc/rc.ae`). `rc_new`/`rc_clone`/`rc_get`/`rc_count` over an atomic count cell; the generic destructor releases the slot exactly once when the last handle drops. An `Rc` value copies like a handle; the count tracks the live handles.
- **`Box[T]` reads through the raw slot** (`std/alloc/box.ae`): `box_new` allocates and moves any `T` in, `box_free` drops the pointee in place then frees, and `box_slot` hands the raw pointer to an `unsafe` reader.
- **Bare generic calls infer composite parameters** (`box_get(&b)` on a `Box[Point]` infers `box_get[Point]`; enum payloads infer the same way) — explicit type arguments remain valid, and a mismatched composite keeps the write-its-type-arguments diagnostic.
- **An index may follow a call result** (`raw.deref()[0]`, `len(raw.deref())`) — heap views read unbound, byte-identically across the three execution routes.
- **The interpreter answers frame-slot pointer operations**: scalar `ptr_drop_in_place` (reads nothing, returns 0) and `ptr_write` through `&v as *mut T` slots.

### Added — language

- **`unsafe` blocks and `unsafe fn`** (M2.12, ADR-0207). Dereferencing a raw pointer and calling an `unsafe fn` require an enclosing `unsafe` block; an `unsafe fn` declaration requires a doc comment naming its preconditions. `unsafe fn` bodies do not behave like implicit `unsafe` blocks.
- **Raw pointers** `*const T` / `*mut T`. Cast from a borrow with `&v as *const T` / `&mut v as *mut T`; read with `p.deref()` / `p.read()` and write with `p.write(value)` inside `unsafe`. A pointer to an owned value is rejected at the declaration — its borrow is untracked.
- **`extern "C"` declarations** — call C symbols at link time (`extern "C" fn abs(v: i64) -> i64;`). Calling a declaration requires `unsafe`. The compiler emits a real C prototype for every declaration, so a foreign symbol works regardless of definition order.
- **`#[no_mangle]` exposures** — C calls ae: `#[no_mangle] extern "C" fn ae_add(a: i64, b: i64) -> i64 { ... }` is emitted as an external symbol; its body is safe ae code.
- **`#[repr(C)]` structs** — fixes field order and C padding rules; required for any struct crossing the FFI boundary by value. Rejected on generic structs and structs with owned fields. Layout is pinned by the compile-time queries `c_size_of[T]()` and `c_offset_of[T]("field")`, evaluated to constants.
- **Channel cancellation** (M4.3.05). `chan_select` ranks a closed arm above an arm that merely holds a value, so closing a dedicated channel cancels a waiting receiver without consuming anything: the pending value stays in the channel for the next receiver, whether it was buffered or parked in a rendezvous send (`spec/concurrency.md` §11).
- **Thread failure propagation** (M4.1.06, ADR-0217). A failure inside a spawned body stops only that thread and is reported by the `wait` that joins it: same `ae:<line>:<column>: <message>` line and exit status 1 as an immediate abort, after the join completed, so siblings joined first finish their work. A failure no one waits for still ends the process at exit.

### Added — diagnostics

- `E0700` — dereferencing a raw pointer outside `unsafe`.
- `E0701` — calling an `unsafe fn` or an `extern "C"` declaration from safe code.
- `E0703` — passing a non-`#[repr(C)]` struct across an `extern "C"` boundary.
- `E0704` — an `unsafe fn` without a doc comment.
- `W0702` — `unnecessary unsafe`: an `unsafe` block containing no unchecked operation (advisory; compilation succeeds).

### Changed — borrow checking (M2.5, ADR-0205)

- **NLL lifetimes**: a persistent view's lifetime now ends at its **last read** instead of the end of scope — whole-variable writes, moves, and `&mut` calls that happen after the view's last use compile; programs rejected under the old block-scoped rule may now be accepted.
- **Field-granular places**: a field write only conflicts with a view of that very field (`p.note = "y";` while `p.name` is viewed is allowed).
- **Views bind once**: a view binding is declare-only — mutable view bindings and view reassignment are rejected at parse time.
- **`&mut` lending calls are release sites**: a call lending a struct mutably releases the viewed bytes of that struct at the call point, decided by the ordered release walk.

### Changed — toolchain

- **`ae emit-resolve`** (ADR-0227/0228, R3b + R3c step 1). A resolver pass over the shared grammar AST prints a deterministic name-resolution dump: one def id per item and local, occurrences with the role and target they resolve to, and the resolver's diagnostics. `tests/resolve` snapshots pin the format. The dump includes a classified `types:` section for every declarative type spelling, and `use` headers are followed transitively so imported `pub` items resolve. The resolver also enforces the compiler's writability model at name level (`cannot assign to immutable variable` for rebinds of non-`var` bindings and writes through them, with `&mut` / `*mut` parameters and `&mut self` receivers accepted). Two shared-grammar fixes came out of the wider corpus: `name[expr] {` no longer misreads an indexed value as a generic literal, and an `&&`/`||` condition keeps its no-struct position across infix right operands (`if b > 0 && a > 100 - b {`). The shared declaration grammar now accepts the receiver spellings `self` / `&self` / `&mut self` (with an empty type range), which impl methods need.
- **Frontend reuse cache** (ADR-0177): `build`/`run` store generated C per source-unit set keyed by SHA-256; rebuilding unchanged sources skips lexing, parsing, checking, and code generation entirely.
- **Windows argv encoding**: program arguments are re-derived from the UTF-16 command line, so `args()` yields UTF-8 on every ANSI code page; Chinese and other non-ASCII paths passed on the command line work directly.
- **Runtime traps are uniform**: a trap raised by the MIR interpreter prints the exact runtime format (prefix, position, message), and all four execution modes (interpreter / `-O0` / release tree / release MIR) must trap identically for the same program.

### Fixed

- `try_create_dir` in the MIR interpreter creates every missing path level.
- Error-path releases survive a clean walk: the `?` propagate binding revives even when the move walk looks clean (ASan-verified leak fix).
- `extern` prototype parameter names are numbered per index, so multi-parameter prototypes no longer collide.
- `W0702` no longer fires on `unsafe` blocks whose unchecked operation is a call to an `unsafe fn` or an `extern "C"` declaration — the same operations E0701 legalizes. Previously the block was reported as decoration on any cold-cache build; warm frontend-cache hits masked it.
- Ten pre-existing compiler warnings eliminated (`PtrStore` unused parameters, dead fields, unnecessary `pub(super)`).
