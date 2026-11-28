# SERVER-STATUS — server relay (SERVER-COORDINATION)

- Date: 2026-11-28 (+08:00); relay session opened 17:24, still running.
- Host: `instabox-node3` (instabox-evaluate-2, 4 cores, shared with other
  sessions' benchmarks and fuzz campaigns — load averaged 8.5-12 through
  this run; at times 5 diff_fuzz workers from other sessions were live).
- Relay checkouts (fresh `git archive` extractions of `origin/main`, each
  with a private in-tree `target/`, sources force-touched before every
  build per the standing mtime lesson):
  - `/data/home/ae-relay/` — segment s1, commit `9907ce26`
  - `/data/home/ae-relay2/` — segment s2, commit `9bfc9f86`
- Fresh binary pinned per segment via
  `AE_FUZZ_BIN=/data/home/<dir>/target/debug/ae`.

## Suite results (Linux, rustc 1.99.0 + gcc 14)

| Segment | Commit | Result |
|---|---|---|
| s1 | `9907ce26` | 695-class workspace: **693 passed / 2 failed / 3 ignored** across all 49 test binaries (`--no-fail-fast`, log `suite2.log`); gates green incl. corpus gate, diagnostic parity, route coverage |
| s2 | `9bfc9f86` | **695 passed / 0 failed / 3 ignored**, all 49 binaries ok, exit 0 (log `/data/home/ae-relay2/suite.log`) |
| s3 | `a35c8de9` | **695 passed / 0 failed / 3 ignored**, all 49 binaries ok, exit 0 (log `/data/home/ae-relay3/suite.log`) |

### The s1 failures were stale pins, already fixed by slice 101

The two s1 failures were in `tests/execution.rs` negative assertions that
still pinned `c_size_of[string]()` / `c_align_of[string]()` as
must-be-rejected, while `f70a8c63` (P4.1 slice 9) had deliberately
admitted string layout queries (ledger pins 24/8):

```
---- c_alignment_matches_the_native_c_compiler stdout ----
thread 'c_alignment_matches_the_native_c_compiler' panicked at tests/execution.rs:5377:9:
accepted: fn main() { print(c_align_of[string]()); }

---- scalar_size_queries_in_generic_and_concrete_contexts stdout ----
thread 'scalar_size_queries_in_generic_and_concrete_contexts' panicked at tests/execution.rs:5405:9:
accepted: fn main() { print(c_size_of[string]()); }
```

Server reproduction at s1: fresh binary `ae run` prints 24 for
`print(c_size_of[string]())`; the stale 04dff421 binary rejects
("'string' is not a struct; the layout queries name concrete structs").
Slice 101 (commit `875469da`, in s2) migrated the pins to positive
assertions (24/8) — the s2 suite above is the Linux proof they pass.
Last local full suite had run at 04dff421, which is why the mismatch
surfaced first here.

Context: the other session's suite on the stale 04dff421 tree
(`/data/home/ae-r2/campaign2.log`) shows exactly 1 failure
(`declaration::tests::reads_attributes_on_structs_and_foreign_definitions`)
— the attribute-order bug `bf4dac28` fixed. Stale-tree artifact only.

## Differential fuzz (full untruncated logs, failures grep-able)

| Segment | Mode | Seeds so far | Failures | Status |
|---|---|---|---|---|
| s1 (`9907ce26` binary) | pure-subset 20k target | **4752** | **0** | stopped early (see note) |
| s2 (`9bfc9f86` binary) | pure-subset 20k | 2194 ok | 0 real / 1593 environmental-ENOSPC | stopped at ~seed 3787 (incident below) |
| s3 (`a35c8de9` binary) | pure-subset 20k → mixed 20k | queued | — | campaign restart on newest main for comparable seed numbering |

### The s2 ENOSPC incident (why the s2 chain stopped)

At s2 seed ~1987 every subsequent compile died instantly. All 1593
FAILURE lines carry `(0 lines)` — a compile-stage death — and **zero**
have non-zero line counts, i.e. no run-stage divergence hides among
them. Root cause verified on the host, not a timeout theory:

```
tmpfs           3.9G  3.9G  8.0K 100% /tmp
ae: writing generated C: No space left on device (os error 28)
```

`/tmp` (3.9 G tmpfs) filled up — TemporaryDirectory trees leaked by the
SIGTERM-killed orphan campaigns (the relay's own kills among them)
stacked on the live campaigns' workdirs. Accounting per the triage
guidance: the clean metric is **2194 ok seeds / 0 real failures**
(s2 pure, `9bfc9f86`); the 1593 FAILURE lines are environmental
compile-stage deaths with the df + probe excerpt above as proof. The
relay chain was stopped (its remaining seeds would all die the same
way), leaked tmpdirs were deleted only after verifying zero live
process references (each dir's /proc scan showed only the diagnostic
shell), and /tmp recovered to 54% used with the binary probe passing
again (`PROBE_EXIT=0`).

Seed-numbering note: main's generator gained the kind-31 heap-string
and kind-32 map-view shapes after `9bfc9f86`, so s2's seed stream is
not comparable to main's. The s3 restart on `a35c8de9` restores
comparable numbering and picks up the fixture migrations (including
`6da1640a`, which fixes the heap_map expected literal below — verified
present in the s3 tree).

Observed (NOT relay's): r2 stale-tree campaign2 (04dff421), r3 campaign3
on current main, one `--duration 1` mixed fuzzer (exited on its own
~14:5xZ when its hour elapsed). Process coordination per the main
agent's explicit authorization (~15:0xZ): killed the duplicated orphan
fuzzers by exact PID after verifying each PID's cwd/cmdline —
`1688745`/`1688746`/`1697459` (ae-r3 relaunch duplicates, current-main
surface), `1688708` (campaign2's mixed leg, cwd `/data/home/ae-r2`,
stale 04dff421 binary — no ADR-0242 value), and `1808021` (the relay's
own orphaned s1 mixed leg that survived the s1 chain kill, at ~66
seeds). The s2 chain is now the only large campaign besides r2's
leftovers; freed cores go to the s2 totals. Killed segments' partial
seed counts are lost (diff_fuzz prints summaries only at completion).

## Sanitizer leg

`python3 scripts/verify_generated_c.py --sanitize --leak` (legacy 41
fixtures, gcc + libasan/lsan):

- s1: **41 PASS / 0 FAIL**, every fixture `leak blocks=0 bytes=0`
  (`/data/home/ae-relay/sanitize.log`).
- s2 rerun (the case list grew to 48 with the heap-shape additions):
  **47 PASS**, all with `leak blocks=0 bytes=0`, and **1 harness
  failure** (`/data/home/ae-relay2/sanitize.log`) — fixed on main, see
  below:
- s3 rerun on `a35c8de9` (case list now 49): **49 PASS / 0 FAIL**,
  exit 0, every fixture `leak blocks=0 bytes=0`
  (`/data/home/ae-relay3/sanitize.log`) — the fixed heap_map entry
  included.

  ```
  AssertionError: ('tests/run/heap_map.ae', 0, b'true\n10\n0\ndone\n', b'[leak] blocks=0 bytes=0\n')
  ```

  Reading the tuple: returncode 0, program stdout `true\n10\n0\ndone\n`
  (byte-identical to the committed `tests/run/heap_map.stdout` modulo
  CRLF), stderr is the leakcheck line with **blocks=0 bytes=0** — the
  program is correct and leak-free. The failure was a stale expected
  literal in the harness: `scripts/verify_generated_c.py` line 239 pinned
  `("tests/run/heap_map.ae", b"true\n0\ndone", True)` — the fixture's
  output changed when slice "map writes through the borrow"
  (`664a314e`) added the second `map_inc` (7+3 → 10), and the case list
  was not updated. **Fixed on main by `6da1640a`** (the s3 tree carries
  `b"true\n10\n0\ndone"` at line 239; verified on the host).

New fixtures' emitted pure C compiled individually with
`gcc -std=c11 -O0 -fsanitize=address,leak`,
`ASAN_OPTIONS=detect_leaks=1`:

| Fixture | Result |
|---|---|
| `box_string.ae` | exit 0, no leak report, stdout matches fixture |
| `ptr_string_view.ae` | exit 0, no leak report, stdout matches fixture |
| `closure_capture_strvec.ae` | exit 0, no leak report, stdout matches fixture |
| `rc_shared.ae` | LSan: 16 bytes in 2 allocations — the `ae_cell_new` count cells of handles still alive at main's end; payload slots freed correctly. Matches the documented process-lifetime count-cell exemption (slices 84/92). With `detect_leaks=0`: exit 0, stdout matches. |
| `generic_inference_composite.ae` | LSan: 9 bytes in 1 allocation — the `box_new(9)` slot (`ae_std_alloc` deliberately mallocs size+1, so 8→9) owned by the still-live local `n` at exit. Bisect proof: the enum/string match path alone is clean (exit 0, no leaks); adding explicit `box_free(n)` clears the leak (exit 0). Same process-lifetime class, not a backend bug. With `detect_leaks=0`: exit 0, stdout matches. |

stdout comparison note: committed `tests/run/*.stdout` files carry CRLF
line endings; emitted programs print LF. Byte-equal modulo line endings
for all five (checked with `diff` after stripping CR).

## Session lessons (relay-specific)

1. `pwsh` is absent and Windows PowerShell 5.1 mangles the Chinese repo
   path, so `scripts/agent-worktree.ps1` cannot run from Git Bash on this
   machine; the relay worktree was created with plain `git worktree add`.
2. `.cargo/config.toml` is untracked (`.git/info/exclude`), so a server
   tarball extraction has no `target-dir` override and lands on the
   default in-tree `target/` — private per checkout. This keeps relay
   trees clear of the shared `/data/home/_cache/ae/target` other sessions
   build into, and makes the fresh-directory + touch procedure trivially
   safe.
3. The ZCode sandbox imposed an ssh egress cooldown (~10:05Z→13:25Z).
   All server workloads run under `nohup`, so nothing was lost.

## Outstanding

- s2 fuzz chain totals (pure + mixed) and the s2 sanitizer rerun result —
  amend this file and push again as they land.
- Fuzz accumulation toward the ADR-0242 50k-per-mode target: combined
  evidence so far on Linux = s1 pure 4752 + r3 segments + s2 in flight.
  The remaining precondition after suites went green on both hosts.
