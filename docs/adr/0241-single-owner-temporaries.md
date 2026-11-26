# ADR-0241: Owned temporaries have exactly one owner in the pure lowering

- Status: Accepted
- Date: 2026-11-27
- Associated Milestone: P3.2 (pure MIR C backend parity)

## Context

The pure lowering materializes a call or intrinsic result in a temporary
local and then, historically, copied that temporary into a second slot when
an enclosing expression or a CFG `Declare` consumed it. Copying an owned
value (`String`, `Vector`, `Map`, a struct or an enum) copies the C header,
including its `owned` flag and buffer pointer, so two MIR locals name one
allocation. Release helpers are idempotent only for the local they free, not
for its copies: whichever copy the generated C releases first frees the
buffer, and the other copy either dangles or leaks if it is never released.

The end-to-end fixtures and the examples corpus stayed correct because the
extra copies were rarely read after the first release, but an AddressSanitizer
sweep of the example programs exposed both faces of the bug: a shallow copy
of a field (`print(r.buf)`) was released by the read-only temporary rule and
then freed again by the owner's drop, while temporaries that no rule claimed
(`write_file(path_join(...), ...)`, `?` propagation payloads) leaked.

The language has one owner per value. The lowering must preserve that
invariant when it introduces internal temporaries: a temporary is the owner
while it lives, and consumers either take it (moving positions), read it and
release it (read-only positions), or leave it alone (places the program can
see).

## Options

| Option | Pros | Cons | Resource Cost (Compile/Runtime/Memory/Disk) |
|---|---|---|---|
| Track an `owned` bit on every MIR local | Explicit; releases become a type-checked pass | MIR shape change across three backends; every producer must set it; the interpreter and legacy MIR emitter cannot infer it from C-shaped values | High |
| Kept copies everywhere, dedupe releases by pointer | No lowering change | Runtime pointer bookkeeping, double-free still possible on interleaved copies, cannot distinguish a field copy from an owner | Medium runtime |
| Single-owner temporaries in the lowering (chosen) | Fits the language rule; no MIR change; the C backend's existing take/clone helpers already express it | Every consumer path must state its position (take, read-and-release, or borrow) | Low compile |

## Decision

1. **A call or intrinsic result is its temporary.** `lower_operand` and
   `lower_operand_moving` return the temporary the callee lowered into
   instead of copying it into a second local. A moving position wraps the
   same place as `Operand::Move`, so the C operand helper takes the value
   (`ae_*_take`) rather than cloning it.
2. **Read-only consumers release what they own.** `lower_readonly_operand`
   reports the owned temporary together with the operand; `print`, string
   comparison, `len(...)` over a materialized container, and a view argument
   of a read-only intrinsic push the drop after the read. The emitter's
   `*_temp_releases` list covers the IO arms that consume a path or content
   operand (`read_file`, `write_file`, `path_join`, `path_name`,
   `path_parent`, `split`).
3. **Field and dereference reads are places, never copies.** `lower_operand`
   returns the projected place for `Expr::Field`/`Expr::DerefPtr`; a shallow
   copy of a field header would give a release helper a buffer it does not
   own.
4. **Propagation moves its payload.** A `?` binding takes the payload out of
   the subject enum (`Operand::Move` on the downcast field) so the binding is
   the single owner the enclosing scope releases; the subject enum keeps a
   zeroed slot and its own conservative release stays a no-op. This replaces
   the earlier string-view binding, which left the enum owning the buffer
   and the view unable to free it.
5. **Argument slots are not owners.** The frontend may hoist an owned
   argument into a CFG `Declare` slot. The lowering assigns the temporary
   into the slot without a second copy (`_22 = copy _21` disappeared when the
   operand started reusing `_21`); a read-only intrinsic that receives the
   slot's value releases it after the call. The slot itself is never treated
   as a long-lived owner, so a later move takes an already-zeroed slot.

6. **A thread context carries borrow captures as views.** A spawned
   closure whose body reads parent storage gets a view, vector reference or
   struct reference of the parent slot in the same thread context, right
   after the moved value captures; the join keeps that storage alive while
   the thread reads it. Task/scope/pool spawns take no borrow captures by
   language rule (the frontend rejects them: `task_spawn moves its captures
   into the task; borrow captures are not allowed`), so every spawn the
   pipeline accepts carries exactly the captures its context packages.

## Evidence

`examples/copy_notes.ae` leaked both faces at once: a `len(lines(text))`
temporary vector and two `path_join` paths passed to `write_file`. After the
rule: an AddressSanitizer+UBSan sweep of the 21 feature examples in Ubuntu
WSL reports **21/21 clean** (exit status 0, 1 or 2 as designed), where the
same sweep previously failed on `copy_notes`. The `tests/run` end-to-end gate
stays **161 of 161** with zero output mismatches, `cargo test --workspace
--locked` reports **686 passed / 0 failed**, clippy is clean, and
`python scripts/verify_generated_c.py` passes **41/41**. The differential
fuzzer compares **300 seeds / 0 failures / 0 skipped** in both the mixed
mode and `--pure-subset`; the seed that first exposed the arity mismatch
(a spawned closure reading a parent string) matches the legacy backend and
is AddressSanitizer-clean. The examples corpus emits **49 of 49** programs.

## Consequences

- A future MIR-level analysis that wants to prove the property must model
  the three consumer positions rather than assume every temporary is
  released; the invariant is documented here because the MIR has no owner
  bit.
- The rule is lowering-local: the legacy tree emitter and the MIR
  interpreter keep their existing ownership discipline, and the differential
  corpus still agrees with them.
- Revisit if a consumer cannot express its position with the current
  `Operand`/`StatementKind::Drop` vocabulary, or if a future backend needs
  explicit ownership metadata; the fallback is the per-local `owned` bit
  (option 1).