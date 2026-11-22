# ADR-0231: Share type rules between resolved expressions and compiler lowering

- Status: Proposed
- Date: 2026-11-22
- Associated Milestone: R3c

## Context

[ADR-0228](0228-type-spelling-resolution.md) records type spellings,
[ADR-0229](0229-type-checker-slice-ladder.md) adds name-level checks, and
[ADR-0230](0230-resolved-function-body-lowering.md) lowers eligible resolved
bodies into the compiler's typed tree. The resolver's inferred spelling is
an editor-facing fact. The compiler's concrete type carries layout and
instance identities consumed by MIR, and the two code paths currently
decide expression types independently.

At `127fb9e`, `ae-resolve::Walker::infer_binary` infers `f64` for
`f64 % f64` and `bool` for ordered comparisons of two booleans.
`ae-compiler` rejects both in `lower_infix`. The accepted-program
differential catches false rejections, but it cannot detect those
disagreements on rejected inputs.

## Decision

Put deterministic type rules in a module of `ae-resolve`, which the
compiler already depends on. The rules take the operator and resolved
operand types and return either a type or a structured failure. An adapter
returns a deferred result when a required type or operation is outside the
implemented domain. Keep that distinct from a checked failure so later
slices can tell inference gaps from rejected expressions.

The compiler owns concrete layouts, generated identities, ownership
effects, and diagnostic locations. It converts its known types into the
rule's domain, runs the rule and constructs the existing typed node.
The resolver uses the same rule when both operand types are known. Keep
its editor-facing spellings and remember that their names alone do not
identify a concrete generic instance. Do not use absence of a spelling as
evidence that the compiler rejects an expression.

## Implementation order

1. **Primitive operators.** Share the rules for `i64`, `f64`, and `bool`
   unary and binary operations, including integer-only bitwise operations
   and remainder. Route the compiler's shared and token-based paths through
   the same operator check. Preserve the compiler's exact message, code,
   and primary token span. In this first slice, rejected operands stop
   resolver inference for that expression. The compiler still owns the
   user-facing error.
2. **Typed expression facts.** Record checked results at expression byte
   ranges in the resolved body. Cover nested expressions and propagate
   deferred results without issuing a second error at each enclosing
   operator. Enable resolver error reporting only for a tested set with
   rejected-program diagnostic parity. Use resolved binding identities
   rather than repeating name lookup when consuming those facts.
3. **Storage and call types.** Migrate array indexing, assignments,
   returns, and direct value arguments. Validate concrete generic
   instances and call signatures with the compiler's existing registries.
   Expand to borrows, aggregate fields and constructors, and ownership
   transitions once the checked result can express their requirements.
4. **Cutover.** Require checked-route coverage per construct for accepted
   and rejected inputs before deleting the corresponding token route.
   Close R3c and M0.2.08 only after the ledger's complete-route and
   diagnostic conditions hold.

## Verification

- Pin successful and rejected primitive combinations at the rule boundary.
  Include mixed numeric types, floating remainder, ordered boolean
  comparison, and unary minus on a boolean.
- Compare the compiler's JSON error code, message, and byte span for each
  rejected probe. At the baseline, boolean ordering yields E0202 and
  boolean negation with unary minus yields E0203. Mixed numeric arithmetic
  and floating remainder carry the compiler's uncoded diagnostics.
- Keep `resolver_agrees_with_the_compiler_on_accepted_programs` green and
  verify that supported shapes reach the checked route. Run the Windows and
  WSL2 workspace suites, the Python suite, and the frontend A/B gate for
  emitted C, MIR, diagnostics, and runtime output before each commit.
