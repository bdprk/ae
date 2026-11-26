# ADR-0242: Retiring the legacy tree C emitter

- Status: Proposed
- Date: 2026-11-27
- Associated Milestone: P3.2 (pure MIR C backend parity)

## Context

Three C emission paths share the compiler today:

1. the **tree C emitter** (`AE_CODEGEN=tree`, the `emit_statements` path in
   `codegen.rs`): walks the typed expression tree and prints C directly; it
   predates the MIR and carries the original ownership bookkeeping. The
   `emit-tree` CLI is a different thing — a text dump of the lowered
   statement tree (ADR-0190) — and stays;
2. the **legacy MIR C backend** (`emit-c`, the default `build`/`run` route):
   lowers the tree into `ae_mir::Body` and emits C from it;
3. the **pure-MIR C backend** (`emit-pure-c`): a separate `mir_pure` lowering
   from the CFG with an independent C11 runtime (`ae_str`, `ae_vec`,
   `ae_slice`, `ae_map`, channels, threads, filesystem, JSON, child
   processes).

Paths 1 and 2 agree byte-for-byte on the accepted corpus and the tree C
path is no longer the default route: only the `AE_CODEGEN=tree` environment
variable reaches it. The differential fuzzer compares all three C outputs
plus the interpreter, and the pure backend now matches every `tests/run`
fixture (164/164) and every example (49/49), including traps, threads,
filesystems, JSON and child processes.

Keeping the tree C emitter costs maintenance on every language change: a new
statement or intrinsic must be implemented a third time, and its ownership
rules diverge from the pure lowering's single-owner model (ADR-0241). Its
remaining value is as a differential reference, which the legacy MIR C
backend already provides for the same corpus.

## Options

| Option | Pros | Cons | Resource Cost |
|---|---|---|---|
| Keep all three emitters | widest reference surface | third implementation forever; only `emit-tree` reaches it | high maintenance |
| Remove only the CLI command, keep the module | one less public surface | dead code still compiles and rots | medium |
| Delete the emitter and its tests (chosen, after the exit gate) | two implementations remain; differential coverage stays through legacy MIR vs pure | loses one independent C spelling as a reference | low |

## Decision

After the P3.2 exit gate is green, delete the tree C emitter:

1. **Preconditions.** Both fuzz modes (`--pure-subset` and the mixed
   generator) reach their 50,000-seed target with zero failures on Windows
   and one Linux host; the `tests/run` gate is 164/164; the examples
   differential gate is 49/49; the workspace suite and clippy are clean on
   both hosts. The evidence lands in `docs/implementation-ledger.md` before
   the deletion commit.
2. **Scope.** Remove the `AE_CODEGEN=tree` switch and the `emit_statements`
   path in `c_output.rs`, the tree-only emission functions in `codegen.rs`
   (keeping the shared `CodegenCtx`, field and float helpers that
   `codegen_mir` uses), the `AE_CODEGEN`-tree mode in `scripts/diff_fuzz.py`
   and the benchmark scripts, and the tests that pin tree-emitter output.
   Keep `emit-c`/`build`/`run` on the legacy MIR path: it is the default
   route and the second independent C spelling the fuzzer compares until P6
   replaces it with a native backend. Keep the `emit-tree` inspection
   command: it prints the lowered statement tree as text (ADR-0190), not C.
3. **Reference preserved.** The fuzzer keeps comparing the legacy MIR C
   output (debug and release), the pure C output and the interpreter; the
   tree C emitter's role as a third C spelling is not replaced. If a future
   regression needs it, git history at this revision is the rollback path.
4. **Documentation.** The implementation ledger, handoff, ADR-0190 context
   and benchmark notes record the removal; the ADR records the final fuzz
   numbers.

## Evidence

To be recorded with the deletion commit: fuzz totals and hosts, the gate
outputs, the suite counts and the commit range that removes the emitter.
Until then this ADR stays Proposed and `AE_CODEGEN=tree` keeps working.

## Consequences

- The compiler loses one independent C emitter; the differential fuzzer's
  comparison becomes legacy MIR debug/release, pure C and the interpreter
  (four binaries), which is the same coverage the examples and corpus gates
  already rely on.
- Every later language feature is implemented twice (legacy MIR and pure)
  instead of three times; P6 removes the legacy MIR path in turn.
- Revisit if the pure backend needs a reference spelling the legacy MIR
  backend cannot provide (for example a backend-specific ABI experiment);
  the tree C emitter can be restored from history for the duration of that
  experiment.