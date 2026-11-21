# ADR-0230: Lower resolved function bodies into the existing typed frontend

- Status: Accepted — the first ladder rungs are landed (see below)
- Date: 2026-11-22
- Associated Milestones: R3c, M0.2.08

**Landed (2026-11-22, dual-platform green per slice):** scalar bodies,
conditionals, while loops, assignments, output statements, entry bodies,
direct and imported calls, local arrays (declarations, reads, stores,
parameters), drop-free struct constructions with the ownership bookkeeping
(scopes, Frees, live_owned Returns, loop-local releases), struct field
reads including nested chains, owned-aggregate calls with move tracking
(E0102, close_loop_moves), and unsafe containers. The remaining rungs are
generic and closure bodies — the recorded design boundary, reusing the
compiler's instance and capture machinery as those bodies move.

## Context

[ADR-0229](0229-type-checker-slice-ladder.md) orders the migration. The
resolver provides definitions, occurrences, classified spellings, and
inferred shapes for a bounded subset. The compiler already consumes many of
these facts through an index keyed by source positions. In particular, it
matches resolved local references to live slots by declaration range and
lowers some shared expressions with deferred ownership effects.

The frontend still dispatches statements from tokens in `block_inner` and
can fall back from shared expressions to `expression_inner`. Closure probes
still invoke the typed body parser, and generic layout instantiation and
trait dispatch still run in the compiler. The byte-parity gate verifies
observable equivalence. It does not establish that these paths consume the
shared tree, so M0.2.08 and R3a remain open.

## Options

| Option | Benefit | Cost |
| --- | --- | --- |
| Expand individual token-site adapters indefinitely | Small local changes | Statement structure still has two authorities and every adapter needs its own cursor contract |
| Replace the frontend in one change | One cutover | Diagnostics, ownership, and generated IDs could all drift at once |
| Lower one complete function-body subset at a time | The new entry point owns a complete body and can be compared against the existing entry point | Both routes coexist during migration and need explicit coverage checks |

## Decision

Use a compiler-owned lowering entry point that takes a resolved function body.
It returns the existing typed statements and expressions. The resolver owns
names, syntax ranges, and classified spellings. The compiler owns concrete
layout types, storage slots, generated IDs, and existing ownership analysis.
Within a function, the new route maps resolver `DefId`s to live compiler
slots as declarations are lowered. References use that mapping, including
a repeated source name bound in a later scope. It must preserve slot order
and the token-path diagnostic span wherever the existing code observes them.

The first implementation slice handles complete non-generic function bodies
whose statements consist of primitive bindings, local reads, and `return`,
with grouped scalar expressions and the existing scalar operators. Parameters
and return declarations must also have primitive types. The route uses the
function's shared statement tree and occurrences to lower the whole body;
it uses the compiler's existing operator typing and emits the same typed
`Function` consumed by MIR. Use a structural eligibility check before any
compiler state changes. A body outside the slice uses the existing entry
point. A body selected for the new route reports its typed errors through
that route, without retrying the same body via token parsing.

Add parity fixtures for arithmetic, comparisons, and a return type mismatch. Assert directly that the new body
entry point is selected for supported fixtures. Whole-corpus parity alone
can pass with fallback, so subsequent slices must also prove route coverage
for the constructs they claim. Grow the body route through control flow
(including repeated names in successive scopes), aggregate construction and access, calls and ownership effects, then the
remaining generic and closure bodies. Reuse the compiler's instance and
capture machinery as those bodies move so their IDs, layout, and capture
order remain stable. Remove the old parser once the supported corpus no
longer depends on it, including rejected-input diagnostics. R3d follows the
existing dependency on the completed frontend cutover.

## Evidence and gates

The baseline for this decision is `3f11b6e`. `block_inner`, `expression_inner`,
`lower_shared_expression`, and `lower_resolved_local` identify the existing
routes in the compiler. `Resolution`, `FnOccurrences`, and `infer_expression`
identify the available facts and current inference boundary in the resolver.
Before each implementation commit, run `cargo test --workspace --locked`,
`python scripts/ab_gate.py --baseline <previous-good-revision>`, the Python
script suite, and `python scripts/wsl_sync.py`. The frontend corpus compares
C and MIR output, CLI diagnostics, and program behavior. Keep the accepted
resolver differential green. Update the ledger only when route coverage
and parity together establish the claimed milestone.

## Consequences

The first slice adds a real typed body route while MIR and code generation
keep their existing input. Subsequent slices can migrate a class of bodies
without changing the resolver's public type model. During migration the
whole-body eligibility decision is the rollback boundary for each slice.
