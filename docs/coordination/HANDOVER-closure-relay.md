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
Direct returns of captured string literals use the same move check.
The borrow probe still fails the owned return type check, so existing
capture inference selects the by-value slot and the closure remains
callable once.
Direct prints of the owned capture, including parenthesized reads, now
use the resolved body without moving the capture before the return.

## What is half-done

Captured strings from other initializers and other uses of an owned
capture still use the typed parser. The next slice can extend capture
classification to resolved string-returning call initializers.

## Next step

```powershell
git status -sb
git log -5 --oneline
```

## Evidence

- Windows and Ubuntu workspace tests each passed 495 tests with no warnings.
- All 21 Python tests passed.
- The byte comparison against `2413230` matched 50 examples and 646 CLI
  invocations. The focused comparator also matched generated C, MIR, and
  output for a closure that reads its capture twice before returning it.
  It matched diagnostics for calling an owning closure twice and for
  reads after moving the captured string to a local binding.
- The Ubuntu generated-C sanitizer and leak check passed for `bounds.ae`.

## Risks and blockers

The original relay worktree is active in another session. This branch
includes its fleet archive commit. Its uncommitted LSP work stays in
that worktree.

## Files touched

- `crates/ae-compiler/src/frontend/parser.rs` implements and tests the
  resolved print path.
