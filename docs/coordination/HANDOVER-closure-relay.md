# Handover — closure-relay

- Date: 2026-11-22 (+08:00)
- Branch: `agent/closure-relay`, based on `2c529c0` from `agent/fuzz-relay`
- Worktree: `D:/用户/trevv/桌面/projects-bdprk-副号/worktrees/closure-relay`
- Areas: parser and docs, released after publication

## What is done

Direct printing of an element of a borrowed string-vector capture lowers
through the resolved closure route as a string view. Capture inference,
capture order, and the integer index check reuse the compiler's existing
representations. Local immutable bindings of indexed views also lower
through the resolved route and register the container borrow using the
same tracking as the typed parser. The parser regression checks that the
resolved declaration and later read use the same local slot.
The resolved return checker reports E0204 when a closure tries to return
a captured string-vector element as an owned string.
Returning a local initialized from a string literal uses the resolved
closure route. The compiler checks the move and records it before the
existing drop pass trims the return's cleanup list.

## What is half-done

Returning captured owned strings and returning strings from other
initializers still use the typed parser. The next slice can admit
captured owned returns while retaining capture inference and call order.

## Next step

```powershell
git status -sb
git log -5 --oneline
```

## Evidence

- Windows and Ubuntu workspace tests each passed 495 tests with no warnings.
- All 21 Python tests passed.
- The byte comparison against `0d3a197` matched 50 examples and 646 CLI
  invocations. The focused comparator also matched generated C, MIR, and
  output for local owned-string returns with a borrowed capture, branch
  returns, and cleanup of another owned local. It matched diagnostics for
  mutable bindings and element returns.
- The Ubuntu generated-C sanitizer and leak check passed for `bounds.ae`.

## Risks and blockers

The original relay worktree is active in another session. This branch
includes its fleet archive commit. Its uncommitted LSP work stays in
that worktree.

## Files touched

- `crates/ae-compiler/src/frontend/parser.rs` implements and tests the
  resolved print path.
