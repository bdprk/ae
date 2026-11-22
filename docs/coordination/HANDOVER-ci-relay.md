# Handover — ci-relay

- Date: 2026-11-22 (+08:00)
- Branch: `agent/ci-relay`, based on `7bdb1fa`
- Worktree: `D:/用户/trevv/桌面/projects-bdprk-副号/worktrees/ci-relay`
- Areas: scripts and docs

## What is done

The Test workflow fetches Git history and passes the pull request event's
base commit to the byte-parity and commit-title checks. The Windows job
runs the generated-C leak verifier after building the compiler.

## What is half-done

Clippy still reports the existing warnings. Strict enforcement requires
its own cleanup. The registry reset in the primary worktree is separate
from this branch and does not introduce program-owned compilation state.

## Next step

```powershell
git status -sb
git log -5 --oneline
```

Review the GitHub Test workflow before integrating the branch.

## Evidence

- Windows and Ubuntu each passed 506 workspace tests.
- The Windows generated-C leak gate passed all 41 programs.
- The baseline matched 50 examples and 646 CLI fixture invocations.
- All 21 Python tests passed.

## Files touched

- `.github/workflows/test.yml` sets the checkout depth, pull request
  baseline, and Windows leak check.
