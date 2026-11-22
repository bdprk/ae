# Handover — reader-relay

- Date: 2026-11-22 (+08:00)
- Branch: `agent/reader-relay`, based on `7bdb1fa`
- Worktree: `D:/用户/trevv/桌面/projects-bdprk-副号/worktrees/reader-relay`
- Areas: stdlib and docs

## What is done

`reader_read_exact` reads a specified byte count across chunk and buffer
boundaries and returns a `ReadResult`. Early EOF consumes the bytes
available. A zero count succeeds with an empty string and a negative
count returns an error. Neither changes the read position.

## What is half-done

The reader still uses an in-memory source. A native hosted reader is a
separate implementation step.

## Next step

```powershell
git status -sb
git log -5 --oneline
```

## Evidence

- The exact-read case produced the expected output with the native
  backend and interpreter on Windows and Ubuntu.
- Windows and Ubuntu each passed 506 workspace tests.
- The comparison against `7bdb1fa` matched 50 examples and 646 CLI
  fixture invocations. All 21 Python tests passed.

## Files touched

- `std/std/io/reader.ae` adds the exact-read operation.
- `tests/run/reader_exact.ae` exercises interleaved line and byte reads
  and the count and EOF cases.
