# SERVER-STATUS — server relay (SERVER-COORDINATION)

- Date: 2026-11-28 (+08:00); relay session opened 17:24, still running.
- Host: `instabox-node3` (instabox-evaluate-2, 4 cores, shared with other
  sessions' benchmarks/campaigns — load averaged 8.5-12 during this run).
- Server checkout: `/data/home/ae-relay/` (fresh git-archive extraction of
  `origin/main`, private in-tree `target/`, sources force-touched per the
  standing mtime lesson before every build).
- Fresh binary pinned for every fuzz/verify invocation via
  `AE_FUZZ_BIN=/data/home/ae-relay/target/debug/ae`.

## Commit segments verified

| Segment | Commit | Status |
|---|---|---|
| s1 | `9907ce26` (test(p4.1): tree nodes over heap pointers) | suite run, sanitizer leg, fuzz campaign started |
| s2 | newest `origin/main` tip (moved 29 commits past s1 during the session: heap vectors, box_slot/rc_slot, ADR-0245 stage 3, fuzzer heap-string shape, interp pointer arms, fmt fixes) | re-sync queued (sandbox egress cooldown 10:0xZ→13:25Z blocked ssh; numbers to be amended on this branch) |

## Workspace suite (s1, Linux, rustc 1.99.0 + gcc 14)

- First run `cargo test --workspace --locked` (fail-fast):
  **execution binary 373 passed / 2 failed**, then cargo aborted the
  remaining targets. Full log kept at `/data/home/ae-relay/suite.log`.
- Fail-fast-off rerun (`--no-fail-fast`, `/data/home/ae-relay/suite2.log`):
  all other targets green, including `pure_e2e_corpus_gate` (5/5 — the
  182-fixture gate), `pure_diagnostic_parity` (137 negatives, 0
  divergences) and `pure_route_coverage`. Aggregate at last poll:
  693 passed / 2 failed / 3 ignored, pending the final binaries.
- **The only 2 failures are stale negative assertions, not compiler
  regressions.** `f70a8c63` (P4.1 slice 9, "owned heap pointees — string
  layout, admissions") deliberately admits string layout queries (the
  ledger pins `c_size_of[string]` at 24/8), but `tests/execution.rs` still
  pins both queries as must-be-rejected:

  ```
  ---- c_alignment_matches_the_native_c_compiler stdout ----
  thread 'c_alignment_matches_the_native_c_compiler' panicked at tests/execution.rs:5377:9:
  accepted: fn main() { print(c_align_of[string]()); }

  ---- scalar_size_queries_in_generic_and_concrete_contexts stdout ----
  thread 'scalar_size_queries_in_generic_and_concrete_contexts' panicked at tests/execution.rs:5405:9:
  accepted: fn main() { print(c_size_of[string]()); }
  ```

  Direct reproduction on the server: fresh binary `ae check` exits 0 and
  `ae run` prints 24 for `print(c_size_of[string]())`; the stale
  04dff421 binary still rejects ("'string' is not a struct; the layout
  queries name concrete structs"). The last full local suite ran at
  04dff421, so these two stale tests never met the new admissions until
  this Linux run. **Decision needed from the main agent:** update the two
  negative assertions to pin `c_align_of[string]()`/`c_size_of[string]()`
  = 8/24 (or re-narrow the admissions); relay did not touch compiler or
  test code.

- Context: the other session's suite on the stale 04dff421 tree
  (`/data/home/ae-r2/campaign2.log`) shows exactly 1 failure —
  `declaration::tests::reads_attributes_on_structs_and_foreign_definitions`
  — which is the attribute-order bug `bf4dac28` fixed. Attributed to the
  stale tree; not evidence against current main.

## Differential fuzz (s1, fresh binary, full untruncated logs)

Campaign script `/data/home/ae-relay/relay-fuzz.sh`: pure-subset first,
then mixed, sequential, `--jobs 2`, logs
`/data/home/ae-relay/fuzz-pure.log` and `/data/home/ae-relay/fuzz-mixed.log`.

- pure-subset `--seeds 20000 --jobs 2`: IN PROGRESS at last poll — 862
  seeds, 0 failures (rate ~0.55 seeds/s under the contested host; the
  chain runs unattended, totals to be amended).
- mixed `--seeds 20000 --jobs 2`: queued behind pure-subset.
- Observed other campaign (NOT this relay's binary): another session runs
  `campaign2.sh` from the stale `/data/home/ae-r2` tree (04dff421):
  pure-subset 20000 then mixed 20000, started 08:59. Left untouched per
  the standing rule. Its totals belong to 04dff421, not to main's tip.

## Sanitizer leg (s1)

`python3 scripts/verify_generated_c.py --sanitize --leak` (41 legacy
fixtures, gcc + libasan/lsan): **41 PASS / 0 FAIL**, every fixture ends
with `leak blocks=0 bytes=0`. Log: `/data/home/ae-relay/sanitize.log`.

New fixtures' emitted pure C compiled individually with
`gcc -std=c11 -O0 -fsanitize=address,leak` and run with
`ASAN_OPTIONS=detect_leaks=1`:

| Fixture | Result |
|---|---|
| `box_string.ae` | exit 0, no leak report, stdout matches fixture |
| `ptr_string_view.ae` | exit 0, no leak report, stdout matches fixture |
| `closure_capture_strvec.ae` | exit 0, no leak report, stdout matches fixture |
| `rc_shared.ae` | LSan: 16 bytes in 2 allocations (`ae_cell_new` count cells of the handles still alive at main's end); payload slots freed correctly; process-lifetime exemption anticipated by the briefing. With `detect_leaks=0`: exit 0, stdout matches. |
| `generic_inference_composite.ae` | LSan: 9 bytes in 1 allocation — the `box_new(9)` slot (`ae_std_alloc` deliberately mallocs size+1, so 8→9) owned by the still-live local `n` at exit. Bisect proof: the enum/string match path alone is clean (exit 0, no leaks); adding explicit `box_free(n)` clears the leak (exit 0). Same process-lifetime class, not a backend bug. With `detect_leaks=0`: exit 0, stdout matches. |

stdout comparison note: the committed `tests/run/*.stdout` files carry
CRLF line endings; the emitted programs print LF. Byte-equal modulo line
endings for all five (checked with `diff` after stripping CR).

## Failures and lessons from this relay session

1. `pwsh` is absent and Windows PowerShell 5.1 mangles the Chinese repo
   path (mojibake), so `scripts/agent-worktree.ps1` cannot run from Git
   Bash here; the worktree was created with plain `git worktree add`.
2. `.cargo/config.toml` is untracked (`.git/info/exclude`), so a server
   extraction has no `target-dir` override and lands on the default
   in-tree `target/` — which is private per checkout. That keeps the
   relay tree clear of the shared `/data/home/_cache/ae/target` other
   sessions build into (no binary-race interference), and makes the
   lesson-1 `touch` + fresh-directory procedure trivially safe.
3. The ZCode sandbox imposed an egress cooldown (ssh blocked
   ~10:05Z→13:25Z). All server workloads run under `nohup`, so the suite,
   sanitizer and fuzz chain were unaffected; polling paused until the
   window reopened.

## Outstanding / next relay steps

- Fill in final pure-subset and mixed fuzz totals (and any failure
  excerpts — full logs are retained server-side, nothing truncated).
- Re-sync `/data/home/ae-relay/` to the newest tip, force-rebuild, rerun
  the suite, extend fuzz totals on the new binary.
- Amend this file with the s2 numbers and push the branch updates.
