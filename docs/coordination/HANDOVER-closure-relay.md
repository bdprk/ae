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

## What is half-done

Uses that require moving the element still use the typed parser. The next
slice should preserve those move diagnostics before widening eligibility.

## Next step

```powershell
git status -sb
git log -5 --oneline
```

## Evidence

- Windows and Ubuntu workspace tests each passed 495 tests with no warnings.
- All 21 Python tests passed.
- The byte comparison against `e9d084f` matched 50 examples and 646 CLI
  invocations. The focused comparator also matched generated C, MIR, and
  output for direct prints and a local view binding, and matched
  diagnostics for mutable view bindings and attempts to move an element.
- The Ubuntu generated-C sanitizer and leak check passed for `bounds.ae`.

## Risks and blockers

The original relay worktree is active in another session. This branch
includes its fleet archive commit. Its uncommitted LSP work stays in
that worktree.

## Files touched

- `crates/ae-compiler/src/frontend/parser.rs` implements and tests the
  resolved print path.
