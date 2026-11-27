# ADR-0244: `string` is a byte container; UTF-8 is checked at the boundaries

- Status: Accepted
- Date: 2026-11-27
- Associated Milestone: P4.3 (hosted standard library and I/O)

## Context

The language's `string` type is an owned byte buffer with a length. The
documented invariant is that a `string` holds UTF-8, and the operations that
would observe a broken sequence check it: `slice` refuses to cut a character
in half (`string index splits a UTF-8 character`), `utf8_len` walks
sequences, and `print` writes the bytes through. Two facts do not fit that
story today:

1. `try_read_file` copies a file's bytes into a `string` without validating
   them, so invalid UTF-8 can enter the type through the filesystem. The
   ledger records this as an open P4.3 gap.
2. The hosted-file test pins the opposite behaviour on purpose:
   `hosted_files_preserve_bytes_and_processes_recover_status` writes
   `a\0b\0c`, reads it back, and compares the bytes, so a NUL-containing
   buffer must survive a round trip.

The two facts together say the current answer is "a `string` is a byte
container whose UTF-8 invariant is checked where a character boundary
matters". That is a real design decision, not an oversight, and it needs to
be recorded before either side changes.

## Options

| Option | Pros | Cons | Cost |
|---|---|---|---|
| Validate UTF-8 in every read | the invariant holds everywhere | binary files become unreadable through the only API that reads them; the round-trip test must be deleted | low compile, breaking |
| Keep bytes; add an explicit byte API (chosen) | both uses are expressible; the invariant is documented and checked at boundaries | a caller can still hold invalid UTF-8 without knowing | low |
| Introduce a separate `bytes` type for buffers | the type system states the difference | a second owned buffer type, its own methods, conversions and codegen paths | high |

## Decision

1. **`string` stays a byte container.** Its documented contract is: the bytes
   are intended to be UTF-8; operations that interpret characters
   (`utf8_len`, `utf8_prefix`, `slice`, `slice_view`, `split`, `lines`,
   `trim`) assume it and fail or trap on a broken sequence exactly as they do
   today. No operation validates eagerly.
2. **File reads stay byte-exact.** `read_file`/`try_read_file` copy the file
   verbatim, so binary content round-trips through
   `write_file` unchanged; the existing hosted-file test is the pin.
3. **The boundary checks are the guarantee.** A program that wants to know
   whether a file is text validates it explicitly with the standard
   library's `utf8` helpers (`std/core/utf8.ae` exposes
   `utf8_sequence_length`, `is_utf8_continuation_byte`, and the batch
   fixtures already exercise them). A future `is_utf8(s)` helper belongs in
   `std/core`, not in the runtime.
4. **Documentation follows.** `spec/` and `docs/current-language.md` state
   the byte-container contract; the ledger's P4.3 gap entry is rewritten to
   record the decision instead of an open hole.
5. **Revisit** if the standard library grows a `bytes` type (option 3); that
   ADR would move `read_file`'s result to it and keep `read_file` returning
   text only when it validates.

## Evidence

- `hosted_files_preserve_bytes_and_processes_recover_status`
  (`tests/execution.rs`) pins the `a\0b\0c` round trip: 5 bytes in, 5 bytes
  out, byte-identical.
- `library_layers_handle_growth_utf8_boundaries_and_map_resize` pins the
  boundary behaviour: `utf8_len("héllo世界") == 7`, `len == 12`, `slice` of
  one character succeeds, and cutting at byte 2 traps with the documented
  message.
- The pure backend mirrors both (`ae_str` carries `ptr`/`len`/`owned`;
  `ae_str_slice` traps on a split sequence; the corpus gate compares the
  trap's position and message).

## Consequences

- No silent semantic change: `read_file` keeps its byte-exact contract and
  the boundary checks keep their messages.
- A caller that needs text validation writes one library call; the runtime
  does not pay for a scan on every read.
- The P4.3 item closes as "decided" rather than "implemented", and the
  ledger says so; a future `bytes` type reopens it under option 3.