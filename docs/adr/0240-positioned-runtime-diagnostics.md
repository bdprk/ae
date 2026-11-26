# ADR-0240: Positioned runtime diagnostics in the pure C backend

- Status: Accepted
- Date: 2026-11-27
- Associated Milestone: P3.2 (pure MIR C backend parity)

## Context

The language makes a runtime trap observable: the process exits with status 1
and stderr carries one line `ae:<line>:<column>: <message>`. Every trap
fixture under `tests/run` pins that line in a `.stderr` file and the status in
an `.exit` file. The main backends have always printed it; the pure C backend
printed `ae trap: <msg>` and called `abort()` because its MIR carried no
source positions at the trap sites. The end-to-end gate therefore skipped
every non-zero exit as a "RUN" reason, and the differential fuzzer excluded
the fifth mode whenever the program trapped.

The MIR already had `span: Option<Span>` on both `Statement` and `Terminator`;
the pure lowering simply never set it. The checked arithmetic helpers, the
bounds checks and the allocator bridge already knew their failure messages,
so the missing piece was the position.

## Decision

1. **One positioned failure ABI.** `ae_fail_at(message, line, column)` prints
   `ae:%zu:%zu: %s` and exits with status 1, exactly like the main runtime.
   `ae_trap(message)` forwards to it with `(0, 0)` for the few sites that
   still have no source position. A child thread records the position in its
   failure record and the observing join reports the same line and status.
2. **Positions travel on existing MIR fields.** The pure lowering keeps a
   `pending_span`; trap-capable expressions (integer binary operations,
   vector and slice indexes, slices, stores, `assert`) set it from their
   AST `(line, column)`, and `push_stmt`/`set_terminator` consume it into the
   statement or terminator's `span`. No `Rvalue` or statement shape changes,
   so the optimizer, borrow checker and evaluator are untouched.
3. **The emitter threads the span to the runtime.** `emit_statement` and
   `emit_terminator` pass `stmt.span`/`term.span` into `emit_rvalue` and the
   intrinsic call. Checked arithmetic helpers take `(size_t line, size_t
   column)`, and the bounds helpers (`ae_vec_at`, `ae_slice_at`,
   `ae_slice_at_mut`, `ae_slice_range`, string slicing, vector pop/store) do
   the same. Their messages match the main runtime's spellings
   (`integer overflow`, `division by zero`, `array index out of bounds`,
   `shift count out of range`).
4. **The gate compares traps.** A non-zero exit with a `.stderr`/`.exit`
   fixture is a real comparison now: the exit status, stderr and stdout must
   all match. Layout probes remain excluded until P6.1.
5. **The fuzzer compares trapped seeds in five modes.** The pure mode no
   longer skips trap seeds; the trap position, message and exit status join
   the agreement contract (the interp's `ae: ` spacing is normalized away).

## Evidence

The end-to-end gate rises 155 → **159 of 161**; the only excluded fixtures are
the two layout probes. The four trap fixtures (`divide_by_zero`,
`vector_out_of_bounds`, `extern_alloc_oom`, `thread_failure`) match stdout,
stderr and exit status byte-for-byte on Windows and WSL. The workspace suite
stays 686 passed / 0 failed, clippy is clean, and the `--pure-subset` fuzzer
compares 300/300 seeds with the fifth mode participating in trap seeds.

## Consequences

- A trap site that has not adopted a position yet prints `ae:0:0: <msg>`
  instead of the old `ae trap:` line; every such site is a follow-up, not a
  silent divergence.
- Positions are statement granularity, which is what the AST gives the
  lowering; an operator's position is exact because `Expr::Binary` carries
  it.
- The gate can now demand 100% of `tests/run` once P6.1 drives the layout
  probes.