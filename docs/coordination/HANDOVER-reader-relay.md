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

`file_reader_new` and `file_reader_read` provide a sequential file cursor.
The native runtime and interpreter both read a bounded byte range and
preserve binary data in an integer vector.

## What is half-done

The existing in-memory reader and the file cursor are distinct APIs.
The file cursor reopens its path on each call. Persistent file handles
and recoverable binary I/O errors are further implementation steps.

## Next step

```powershell
git status -sb
git log -5 --oneline
```

## Evidence

- The exact-read case produced the expected output with the native
  backend and interpreter on Windows and Ubuntu.
- Windows and Ubuntu each passed 506 workspace tests.
- The comparison against `7bdb1fa` matched 50 examples and 650 CLI
  fixture invocations. All 21 Python tests passed.
- The binary file reader passed 507 workspace tests on each of Windows
  and Ubuntu. A repeated-read probe found no additional live allocations,
  including when the requested limit exceeded the file size.
- Against `d5e14d0`, all 50 example entry points and 650 CLI
  fixture invocations matched.

## Files touched

- `std/std/io/reader.ae` adds the exact-read operation.
- `std/std/io/file_reader.ae` adds the binary file cursor.
- `crates/ae-compiler/src/runtime.c` and the compiler frontend implement
  bounded positional reads on both backends.
- `tests/execution.rs` covers binary bytes, the cursor across a read
  buffer boundary, and a path passed as a borrowed view.
- `tests/run/reader_exact.ae` exercises interleaved line and byte reads
  and the count and EOF cases.
