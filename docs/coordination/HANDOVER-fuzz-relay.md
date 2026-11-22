# Handover — fuzz relay

- Date: 2026-11-21 20:39 +08:00
- Branch: `agent/fuzz-relay`
- Worktree: the dedicated fuzz-relay worktree of this checkout
- Area claim: docs

## What is done

- Recorded seven original log prefixes in the implementation ledger and verified output from the restarted evaluate-3, ai and Ubuntu WSL lanes.
- Identified overlapping segments. The Windows one-hour process was kept and the duplicate 3600-hour process was stopped.
- Continued evaluate-2 at 622800, eva at 409536 and evaluate-1 at 515799 after their terminal summaries. Windows resumed at 953000 with a fresh log.
- Added the full frontend differential gate, stabilized generated output and routed the compiler's impl-body parameter handling through the shared method declaration tree (`f8a234a`).
- Continued evaluate-3 at 722356 and ai at 808862 after their zero-failure terminal summaries.
- Ubuntu completed the bounded 929886–949999 segment with zero failures. The next one-hour segment starts at 1000000 and stops before 1100000. Keep Windows below 1000000.
- Bounded statement-tree nesting and switched loop jumps to shared statement nodes before attaching ownership cleanup and loop targets.
- The Windows replay completed all 1,080 missing seeds. The 953000 segment then completed 4,969 seeds with zero failures and continued at 957969.
- Module assembly and resolution now share the entry token stream in `emit-resolve`. The same entry points accept the compiler's existing token slice.
- Literal `print` statements and boolean-literal `assert` statements now lower from shared statement and expression nodes.
- Extended shared lowering to scalar operator trees, using the existing prefix and infix type checks.
- Scalar local reads in shared output trees use resolver definitions that match active compiler slots.
- Scalar binding initializers and return values now use shared expression nodes, including resolved local reads.
- Scalar branch and loop conditions now use shared expression nodes and the existing boolean type check.
- The general expression entry lowers outermost scalar expressions at semicolons, commas, closing parentheses, closing brackets and range separators.
- Nested scalar lowering uses the enclosing expression budget. A call with 128 nodes succeeds and one with 129 retains the existing diagnostic.
- Shared expression lowering now covers string literals and literal-only string operations.
- Bare owned local reads and moves now use resolver definitions and the compiler's existing borrow checks. Compound owned expressions still use typed lowering.
- Safe direct calls to same-module functions with scalar arguments and scalar or string results now lower through resolver definitions and compiler signatures.
- Shared direct calls now accept owned string temporaries and borrow string views from temporaries or resolved locals.
- Shared direct calls stage owned string moves and deferred view conflicts until the enclosing expression lowers successfully.
- All seven lanes were restarted from the next seeds after 93,428 seeds completed with zero differential failures. See the ledger for the ranges.
- evaluate-2 resumed at 634915 after completing 12,115 seeds with zero failures.
- eva resumed at 416112 after completing 6,576 seeds with zero failures.
- evaluate-1 resumed at 527527 after completing 11,728 seeds with zero failures.

## What is half-done

- The remaining active runs still need terminal summaries. Reconcile unique seed IDs across logs before crediting coverage.
- The Ubuntu 916600 relay ended without a terminal summary. Its contiguous observed prefix is recorded in the ledger.
- ADR-0229 slice 5 still needs broader statement, call, and ownership-sensitive lowering before M0.2.08 can close.

## Next step

```powershell
ssh instabox-evaluate-3 'tail -n 20 ~/ae/bench/results/fuzz-segment-start779893.log'
wsl -d Ubuntu -- bash -lc 'tail -n 20 ~/ae/bench/results/fuzz-segment-start1127538.log'
Get-Content ../../ae/bench/results/2026-11-21-distributed-campaign/windows-baseline-start982506.log -Tail 20
cargo test --workspace --locked
```

## Evidence

- The original log prefixes contain 57,351 distinct seed verdicts, with zero differential failures and zero environmental verdicts.
- See the November 21 distributed regression campaign entry for lane starts and initial counts.
- Windows and Ubuntu WSL each passed 482 workspace tests and matched the pre-method compiler byte for byte on 50 generated C examples and 646 CLI invocations.
- Both platforms passed 483 workspace tests and byte parity on the same corpus after the statement depth and loop-jump changes.
- Both platforms passed the same suite and corpus after token-stream reuse. Windows also matched the previous resolver's output on eight `emit-resolve` inputs, including a module with imports.
- Both platforms passed 483 tests and the full frontend corpus after literal statement lowering. Windows also matched six focused inputs covering supported literals and malformed statements.
- Scalar operator lowering passed 483 tests and the full corpus on Windows and Ubuntu. Windows also matched 27 focused invocations, and the Ubuntu candidate passed a 40-seed differential smoke run.
- Resolved scalar reads passed 484 tests and the full corpus on both platforms. Windows matched 24 additional focused invocations, and Ubuntu candidate fuzz covered 40 more seeds without failures.
- Shared scalar initializers and returns passed 484 tests and the full corpus on both platforms. Windows matched 32 focused invocations, and Ubuntu candidate fuzz passed 40 seeds.
- Shared scalar conditions passed the same 484 tests and corpus on both platforms, 28 focused Windows comparisons and 40 candidate fuzz seeds on Ubuntu.
- The general expression route passed 484 tests and the same full corpus on both platforms. Windows matched 28 focused assignment, index, range, call and complexity comparisons. Ubuntu candidate fuzz passed 40 seeds.
- Nested scalar lowering passed 484 tests and the full corpus on both platforms. Windows matched 16 focused invocations, including the 128- and 129-node cases.
- Shared string expressions passed 484 tests and the full corpus on both platforms. Windows matched 24 focused comparisons. Ubuntu generated C passed the 41 sanitizer/leak checks.
- Bare owned locals passed 484 tests and the full corpus on Windows and Ubuntu. Windows matched 24 focused comparisons, including use-after-move and deferred borrow cases. Ubuntu passed all 41 generated-C sanitizer/leak checks.
- Direct calls passed 484 tests and the full corpus on both platforms. Windows matched 28 focused calls and diagnostic cases. Ubuntu candidate fuzz passed 40 seeds.
- Ubuntu generated C passed 41 sanitizer/leak cases with zero leaked blocks.

## Risks and blockers

- Multiple lanes replay seed ranges. Terminal summaries establish completion and elapsed time.
- Ubuntu is the validation distribution. The default docker-desktop distribution fails to mount its VHD.
- The original Windows log was replaced while its producer was live. Use the earlier checkpoint and the completed bounded replay to reconcile coverage.

## Files touched

- `docs/implementation-ledger.md`
- `AGENT_PROGRESS.md`

## Latest validation

- String call arguments passed 484 tests, 50 generated C example comparisons, and 646 CLI comparisons on Windows and Ubuntu. Windows matched 24 focused invocations. Ubuntu passed 41 generated-C sanitizer and leak checks.

- Staged string moves passed 484 tests, 50 C examples, 646 CLI invocations, and 28 focused comparisons on each platform. Ubuntu passed 41 sanitizer and leak checks.

- Deferred string view moves passed the same Windows and Ubuntu corpus and focused checks. The general expression entry now consumes indexed expression roots from the resolver module.

- Resolver expression roots passed 484 tests, 50 generated C examples, 646 frontend CLI invocations, and 28 focused comparisons per platform. Ubuntu passed 41 sanitizer and leak checks and 40 candidate fuzz seeds.

- Indexed output statements and loop jumps passed 484 tests, 50 generated C example comparisons, 646 CLI comparisons, and 32 focused comparisons per platform. Ubuntu passed 41 sanitizer and leak checks. The resolver still parses opaque closure bodies during its walk. Exposing those bodies to compiler lowering is the next frontend step.

- The 15:27 batch covered 78,348 distinct seeds with zero differential failures and zero environmental verdicts. Active successors are eva 429299 (PID 3614912), evaluate-1 550709 (3338406), evaluate-2 658941 (1679986), evaluate-3 745647 (343010), ai 818844 (418711), Windows 970018 (5512, exclusive ceiling 1000000), and Ubuntu 1066059 (guest 913946, exclusive ceiling 1100000). Use the Windows baseline log named above. The separate Windows 970018 probe is excluded.

- The resolver now retains closure statement trees. The compiler uses the same index for regular and closure bodies. Both platforms passed the full frontend corpus and 12 focused closure comparisons, and Ubuntu passed 41 sanitizer and leak checks. Next, lower numeric vector literals from shared expression nodes.

- Numeric vector literals now lower from the resolver tree. Both platforms passed the full frontend corpus and 40 focused comparisons, including valid and invalid complexity-boundary cases. Ubuntu passed 41 sanitizer and leak checks and 40 candidate fuzz seeds. Next, lower resolved numeric array and vector index reads.

- Resolved numeric index reads passed the full frontend corpus and 32 focused comparisons on each platform. Ubuntu passed 41 sanitizer and leak checks and 40 candidate fuzz seeds. Next, share the cached resolver index with closure subparsers that use the same token stream.

- The lazy resolver index is now shared by closure subparsers and same-source generic instantiations. Both platforms passed the full corpus and 32 focused comparisons. Ubuntu passed 41 sanitizer and leak checks. Next, lower numeric vector values and moves at direct-call boundaries.

- Direct calls with numeric vector value parameters or returns now lower from shared trees. Both platforms passed the full corpus and 36 focused comparisons. Next, lower borrowed numeric vector arguments through the resolver.

- Read-only vectors and fixed arrays now lower through resolved borrow nodes. Both platforms passed the full corpus and 44 focused comparisons. Ubuntu passed 41 sanitizer and leak checks. Next, carry statement-scoped struct borrow effects through shared calls.

- Read-only struct arguments now stage statement-scoped borrows during shared call lowering. Both platforms passed the full corpus and 36 focused comparisons. Next, stage mutable struct arguments and preserve deferred view-conflict ordering.

- Mutable struct arguments now lower from shared call nodes. Both platforms passed byte parity and 40 focused comparisons, including mixed move and mutate conflict order. Ubuntu passed 41 sanitizer and leak checks. Next, route owned struct value arguments and returns through the same shared call path.
- At 16:17, evaluate-1 PID 3338406 could not be observed because SSH closed the connection. Recheck that PID and its terminal log before launching the next segment.

- Owned struct value arguments and returns now lower from shared calls. Both platforms passed the full corpus and 40 focused comparisons. Next, lower struct constructor nodes from the resolver tree.

- At 19:25, seven terminal logs covered 78,897 seeds with zero differential or environmental failures. Active starts and PIDs: eva 435878 (1342854), evaluate-1 562107 (3693383), evaluate-2 670902 (2051232), evaluate-3 757011 (686040), ai 823803 (816250), Windows 975869 (34036), and Ubuntu 1092844 (guest 11). The evaluate-3 and ai continuations are the next expected to finish.

- evaluate-3 and ai completed a further 16,254 seeds without failures. Their verified successors are 768241 (PID 1026848) and 828827 (PID 1219720).

- Shared lowering now handles named struct constructors. Both platforms passed the full corpus and 36 focused comparisons. Next, consume field access nodes and preserve the compiler field path semantics. The Ubuntu relay remains bounded at 1100000.

- Scalar field reads now lower from shared postfix trees. Next, handle read-only owned-field expressions while preserving the original move diagnostics. Ubuntu reached its ceiling with 7,156 additional successful seeds and resumed at 1100120 (guest PID 14, host PID 26140), stopping before 1200000.

- Shared string field reads passed the full corpus and 36 focused comparisons on both platforms. Next, consume resolved enum constructor nodes. The seven active relay PIDs were verified live.

- Concrete enum constructors passed the full frontend corpus and 36 focused comparisons on both platforms. Next, handle enum values at shared direct-call boundaries.

- Enum value calls passed the full byte gate and 32 focused comparisons on both platforms. Next, connect shared lowering at the match-subject boundary while preserving pattern ownership.

- Match subjects and arm patterns now reuse the shared index. The full byte gate and 36 focused comparisons passed on both platforms. Next, feed trailing propagation nodes into the typed propagation route.

- Shared propagation passed the full corpus and 36 focused comparisons. Next, consume indexed for-loop operand nodes while preserving iteration mode and ownership.

- Indexed for-loop operands passed the full corpus and 36 focused comparisons. The diagnostic binding-range map retains its prior scope. Next, consume fixed-array initializer trees.

- Fixed-array initializers passed full corpus and focused parity on both platforms. Next, consume indexed assignment targets and preserve the existing write checks. The three early remote lanes and the Windows lane should be checked for completion around 20:25.

- Indexed store indices and keys passed 36 focused comparisons and the full byte gate on both platforms. Next, consume shared field target nodes for typed stores.

- The 20:25 batch recorded 36,698 seeds and zero failures. Verified successor starts and PIDs: eva 442316 (3642392), evaluate-1 573904 (4049278), evaluate-2 682728 (2410459), and Windows 982506 (12116). Next terminal checks are evaluate-3 768241 (1026848) and ai 828827 (1219720) around 20:29.

- Direct field stores now use indexed target names. Next, route pointer-store targets from the statement tree while preserving unsafe checks and existing diagnostics. The 20:29 batch covered 21,260 successful seeds, with verified successors 779893 (evaluate-3 PID 1381435) and 838435 (ai PID 1516911).

- Pointer-store target indexing passed the full corpus and 24 focused comparisons. Next, replace the remaining local statement dispatch for indexed output and jump nodes with shared statement kinds.

- Ubuntu finished 27,418 successful seeds at 20:39. Its verified successor starts at 1127538 (guest PID 14, host PID 9564), with an exclusive ceiling of 1200000.

- Output and loop jumps now select their lowering from shared statement kinds. Both platforms passed the full byte gate and 32 focused comparisons. Next, index shared let and var binding metadata for statement lowering.

- Indexed let and var headers passed full corpus parity and 32 focused comparisons. Next, use the shared for statement metadata for loop binding and iteration dispatch.


## Remaining typed-parse sites (audit 2026-11-21, relay continuation; framing corrected)

The 36 `self.expression(0, …)` sites in `parser.rs` are NOT "unshared" —
each consumes the shared tree indirectly: `expression()` consults
`lower_shared_value` at operand starts, which reads the resolver's
`values` map. The audit's real value is the per-construct location list
for the remaining typed-parse entry points whose operand shapes the
shared lowering does not cover yet. The authoritative work list is the
"Next:" pointer at the end of each handover bullet (the incremental
expansion of `shared_expression_start` and the `values` coverage), not
this site count.

Per-construct locations (for the next slices):
- concurrency statements: TaskYield ×2, PoolSpawn ×1, ChanClose ×2 — the
  chan_close handle now lowers through the shared route (relay continuation)
- stores (value sides): Store ×2, StrVecStore ×1, VecStore ×1 routed
  (2026-11-21 relay); remaining: FieldStore ×6, Star (pointer store) ×1,
  StrVecStore ×1
- compound assignment: PercentEqual ×2; plain Assign ×2
- containers and builtins: Array ×1, VecPop ×3, MapPut ×2, Sort ×1
- control flow tail: condition ×1, Return ×1, For ×2,
  Assert ×1, Print ×1, match_statement ×1
- Declare ×1

Relay continuation (2026-11-21, the takeover agent): the `exit` builtin's
argument now lowers through `lower_shared_value` with an RParen boundary
(Windows 484/0, WSL2 484/0) — the Exit ×2 entry above is resolved, leaving
34 sites.


## Shared-lowering frontier (audited 2026-11-21 21:30)

`lower_shared_expression` (parser.rs:670-910) covers: Atom (9 arms: literals,
ident locals, prelude types), Group, Propagate (via lower_shared_value's
boundary unwrap), Binary, Vector, Struct, Qualified, Index, Field, Call,
BorrowArgument, Prefix (Bang/Minus), For (via lower_shared_iterable at the
call sites), plus lower_shared_array for fixed arrays in the Declare arm.

**Uncovered kinds** (each falls back to the typed parse today):
1. `Propagate` nested inside larger expressions - the boundary unwrap only
   handles the outermost propagate; nesting needs the compiler's Try
   construction with the operand's error type.
2. `AddressCast` / `PointerCast` - the cast construction needs the pointee
   TYPE from the spelling: the compiler parses it with
   `pointer_cast_target`/`named_type` on the cursor; the shared route is
   structure-driven (cursor at the expression end), so the pointee parse
   needs cursor repositioning into the pointee range - fiddly but bounded.
   The ownership rule (owned-pointer rejection) and the untracked-borrow
   semantics must be reproduced exactly.
3. `Specialization` / `BracketedCallTarget` as standalone values - generic
   instantiation needs the compiler's template machinery (M1.7) - stays
   compiler-side until the type checker owns instantiation.
4. `Closure` - deliberately opaque (capture inference is compiler-side).

## Frontier closed (2026-11-22)

Every typed-parse entry point except one now routes through the shared
tree: statement heads (let/var, for), statement values (exit, chan_close,
cell_store, spawn handles, push, array/vec/strvec/map stores, moving
values, compound RHS, return, pointer stores), reads (locals incl.
copy-semantics handles, map keys, array/vec indices, assert/print
arguments, field-store keys). All dual-platform 484/0 with byte parity.

**Body-route ladder status (2026-11-22 05:20, relay night):** the resolved
scalar body route now owns complete bodies with scalar/bindable locals,
arrays (declarations, reads, stores, parameters), drop-free struct
constructions with ownership bookkeeping (scopes, Frees, live_owned
Returns, loop-local releases), struct field reads incl. nested chains,
calls consuming owned aggregates (move tracking with E0102 and
close_loop_moves), and unsafe containers — 18 dual-platform slices since
`2a55df9`. The ladder's next step is the recorded design boundary: generic
and closure bodies (capture inference, instance machinery), then old-parser
removal once the corpus no longer depends on it.

**The single remaining site is by design:** the `spawn` callable
parameter (parser.rs:196). It needs `Expr::ClosureLit(cid)` with capture
inference from the compiler's `generics.closures` machinery; the shared
resolver keeps closure bodies opaque (ADR-0229's recorded decision), so
this argument stays on the typed parse until R3c owns capture inference.

With the sweep complete, the next evaluation is the M0.2.08 close
condition: `emit-c` byte-parity over the whole corpus plus the remaining
typed-only constructs (casts, specializations) reviewed as either
R3c-blocked or acceptable.

The frontier is the type-checker boundary (R3c): the remaining kinds each
need type-level machinery the resolver does not have. The ladder resumes
here in the next session, one kind per slice with the dual-platform gates.



## Standard library layering (2026-11-22)

R4.1 now places allocation APIs in std/alloc, compiler models in
std/compiler, and hosted modules in std/std. The import checker covers
all four ranks. Next frontend slice: the typed spawn callable and its
capture-inference boundary.

## Spawn callable (2026-11-22)

Named closure references use shared expression lowering, including grouped
references. The task fixture exercises a grouped closure with a captured
value. Inline literals still use the compiler's capture inference. The next
expression constructs to review are pointer casts and specializations.

## Pointer retags (2026-11-22)

Shared expression lowering consumes pointer-to-pointer casts to scalar
pointees, preserving the typed cursor location and pointer interning.
Address casts still use the typed path for borrow and ownership checks.
Compound pointee types and generic specialization remain compiler work.

## Address casts (2026-11-22)

Shared lowering handles address casts of scalar locals with the resolver
binding span. Writable addresses require a mutable slot. Mismatched or
non-scalar types and invalid borrows retain the typed parser diagnostics.
Generic specialization and compound pointee types remain open.

## Generic function calls (2026-11-22)

The compiler uses shared type argument ranges for bracketed generic
function calls, including nested calls. It resolves each type and
instantiates the function with the existing compiler machinery. Generic
struct and enum specializations remain on the typed path.

## Generic struct specializations (2026-11-22)

Struct literals and associated calls reuse the shared specialization
argument ranges. The compiler still specializes layouts and checks
visibility. Generic enum expressions are the next consumer.

## Generic enum specializations (2026-11-22)

Generic enum variants and associated calls reuse shared specialization
argument ranges. The compiler retains type argument checks and enum layout
instantiation. Compound pointee types and inline closure capture inference
remain on the typed expression path.

## Compound pointer cast targets (2026-11-22)

The typed cast path consumes the shared pointer qualifier for compound
and named pointees. It resolves each pointee once at the active cursor,
where generic layout instantiation and diagnostics preserve their order.
Inline closure header consumption is the next frontend slice.

## Inline closure headers (2026-11-22)

The closure compiler consumes the shared parameter names, binding spans,
and signature boundaries, including headers of closures passed to spawn.
Capture discovery and typed body lowering still run in the compiler.
The M0.2.08 closure audit still needs to account for typed body parsing.

## Typed body lowering audit (2026-11-22)

[ADR-0230](../adr/0230-resolved-function-body-lowering.md) records the
remaining token dispatch and selects complete primitive function bodies as
the first typed lowering slice. R3c and M0.2.08 remain open.

## Resolved scalar function bodies (2026-11-22)

The compiler now selects complete flat bodies for primitive bindings and
returns and lowers their references through DefId-to-slot mappings. The
typed body route reports return mismatches directly. Control flow and
scoped bindings are the next body-level slice.

## Resolved scalar conditional bodies (2026-11-22)

The resolved body route lowers if, else if, and else. Branch-local slots
leave scope at the end of their branch, allowing the same name in sibling
branches. Scalar loops and their exits remain a separate slice.

## Resolved scalar while loops (2026-11-22)

The resolved body route now lowers while, break, and continue, including
nested loops and the outside-loop diagnostic. Scalar assignment and range
loops are still on the typed token route.

## Resolved scalar assignments (2026-11-22)

Scalar bodies now lower plain and compound assignment to resolved locals.
Writable bindings and compound operand types retain the compiler's existing
checks. Range loops remain the next control-flow slice.

## Resolved scalar range loops (2026-11-22)

The scalar body route lowers i64 ranges, declares bounds in evaluation
order, and uses the existing range-loop continue label. The loop binding
is scoped to its body. Iterable collection loops remain open.

## Resolved scalar print and assert (2026-11-22)

The resolved body route lowers scalar print operands and boolean assertions.
Assertion nodes keep the keyword's source position and type errors keep the
existing operand-end diagnostic. The next slice can widen body entry to main.

## Resolved scalar main bodies (2026-11-22)

Eligible main bodies now use the same resolved body entry as scalar
functions. Main still rejects explicit return statements at the keyword.
Direct scalar function calls are the next body-level expression slice.

## Resolved scalar direct calls (2026-11-22)

Scalar bodies now lower direct calls through resolved same-module function
signatures. Arguments lower left to right. Calls with non-scalar or
external signatures still use the typed token path.

## Resolved cross-module scalar calls (2026-11-22)

The scalar body route uses loaded public signatures to complete call names
left open by file-local resolution. Private functions and FFI calls retain
the existing typed path.

## Resolved local arrays (2026-11-22)

Local array declarations and index reads use resolved binding identities in
the body route. Element and index type checks retain their source spans.

## Resolved array element stores (2026-11-22)

`a[i] = v` on a resolved local array lowers through the body route as
`Statement::Store`, with the typed route's checks mirrored: the binding must
be mutable (E0103), the index and the element must be i64 (E0201 for the
index). Compound operators (`+=`) stay on the typed route, which rejects
them for arrays.

## Resolved array parameters (2026-11-22)

Array parameters (`&[i64; N]` — the only array parameter form) flow through
the body route: the route entry guard now accepts `Type::Array` slots, and
the resolver's parameter definitions join `array_definitions` (an array type
spelling starts with `[`, where vec/map/composite spellings start with an
identifier — the resolver keeps the pointee opaque, so the spelled tokens
decide). Element reads lower as `Expr::Index`; stores on the immutable
parameter are rejected with the typed route's exact E0103 diagnostic. Borrows
of the array (`&a`) and calls to array-taking functions stay typed: a borrow
occurrence never lands in `scalar_reads`, and `supports_call` excludes
non-scalar signatures.

## Resolved struct field reads (2026-11-22)

Field reads on file-local struct parameters join the body route
(ADR-0230's aggregate-access step, first slice). Only immutable borrowed
parameters (`&Point`) qualify: by-value and `&mut` structs carry ownership
effects — moves and drop at scope exit — that stay typed (the
`user_defined_drop` execution test pins this). The resolver's inferred
spelling names the struct item, the compiler's registry supplies field
types, and only scalar fields (i64/bool/f64) mark a read eligible. Nested
expressions are reached by a local deep walker (`visit_expression_tree`),
because `statement.expressions()` stops at top-level values. Field stores,
generic instances, imported structs, method calls and whole-value reads of
the struct stay typed. Nested chains (`o.inner.x`) lower recursively:
struct-typed intermediates are chain-only receivers (a chain set distinct
from the scalar reads), and the owner struct comes from the declared type
of the field it reads.

## Resolved drop-free struct constructions (2026-11-22)

`let p = Point { x: 1, y: 2 };` joins the body route with the ownership
bookkeeping: block scopes push and pop in `lower_scalar_statements`, owned
slots are released at scope exit (`Frees` after the statements),
`Return` carries `live_owned()`, and loop-local `Frees` precede
`Break`/`Continue` (`loop_scopes` bases tracked; the block-depth guard
mirrors the typed route). Constructions are marked only when the shape is
complete, the struct is file-local and non-generic, `drop_fn` is None and
every declared field is scalar — malformed shapes and drop-carrying structs
(the `user_defined_drop` semantics) stay typed with canonical diagnostics.
Local bindings of routed structs are field-read receivers, so `p.x` works
on constructed values.

## R3d started: hover and definition on the def table (2026-11-22)

ADR-0229 slice 6's first slice: the language server resolves open documents
lazily (lex + parse_module_tokens + resolve_with_tokens, cached per
document version) and answers hover from the occurrence under the cursor -
functions render parameter/return spellings with named defs as item names,
structs render fields, bindings render inferred types; definition jumps to
the def's name range. Imported defs (other-file ranges) and
builtin/prelude names fall back to the existing text heuristics. Rename is
scope-exact through the def table (the def's name range plus every
occurrence targeting it; documents the shared parse rejects fall back to
text). Definition crosses files: the document cache carries the
disk-loaded imports, an imported def maps to its own file by name match
(an imported declaration's range spans its whole text; the def's range is
the name token import_name_range found), and the location uses the import
file's own line index. Completion
merges the enclosing function's locals (declared before the cursor, with
inferred types) and the file's functions/structs with their declared
shapes; exact live-scope filtering needs scope-structured ranges and
stays a later slice. The document cache carries the module for the
enclosing-function lookup. Function return spellings are found by role
(SpellingRole::Return) — the entry's name field is empty. Note: the
shared module parse requires `-> Ret` on non-main functions (the
language's own rule), so documents with bare `fn a() { ... }` have no
resolution and fall back.

## Resolved unsafe blocks (2026-11-22)

`unsafe { ... }` is a scoping container: the body route wraps the lowered
statements in `Statement::Unsafe` while mirroring the typed route's
`unsafe_depth` tracking and the W0702 stderr note for blocks with no
unchecked operation. Blocks containing unchecked operations keep their
bodies ineligible, so pointer stores and extern calls stay typed.

## Fleet cooldown (2026-11-22 02:30-04:30)

The InstaBox free plan suspended all five sandboxes mid-segment
("administratively prohibited ... wake again at ..."). Supervisors resumed
on wake; the interrupted segments left no completion lines, so the
35-segment archive stands. Certificates renewed to 2026-11-23 04:33.
Long-fuzz sessions should schedule idle gaps to avoid the throttle.
The cooldown killed the supervisor chains outright (processes die, not
pause). Recovery at 06:05: a per-box `~/segment_chain.sh` (1-hour segments,
per-segment logs, a 10-minute idle gap between segments, seeds resuming at
last-start + completed-count) relaunched with nohup; eva 448887,
evaluate-1 585527, evaluate-2 694695, evaluate-3 791564, ai 848504, jobs 3
(ai 2). **pgrep checks must use the `[x]` self-match-proof form** — the
plain pattern matched the probe itself and reported phantom processes for
two hours. Harvest completed segments by the same seed-arithmetic the
chain uses.

## Resolved owned-aggregate calls (2026-11-22)

`consume_point(q)` with `q: Point` a routed local joins the body route:
`supports_call` accepts owned-struct parameters, the argument position
re-admits struct idents (a move, not a value read), and the call moves the
argument into the diagnostic-only `moved` set - later reads reject with the
typed route's exact E0102 hint and `close_loop_moves` guards loop bodies
against moving outer values. The Frees lists stay raw: the drops pass trims
moved slots for both routes alike. Borrowed-aggregate arguments (`&p`) and
bare effect-only call statements stay typed.

## Resolved scalar generic instances (2026-11-22)

Generic function bodies now enter resolved lowering when their concrete
substitutions are scalar and the body satisfies the existing eligibility
checks. The compiler allocates and caches instances before and after body
lowering as before. Non-scalar substitutions and generic methods still use
the typed path. Closure bodies are the next instance-boundary slice.

## Resolved emitted closure bodies (2026-11-22)

After the compiler fixes the capture modes and slot order, eligible closure
bodies use the resolved body route. Capture probes retain their existing path.
The emitted body maps capture definitions to their assigned slots. Closure
shadowing conflicts and unsupported capture types retain the typed path.

## Resolved generic associated bodies (2026-11-22)

Receiver-free associated functions on generic impls can use resolved body
lowering after scalar substitution. Method instance allocation and caching
stay with the compiler. Receiver methods still require field eligibility
based on the concrete receiver layout.

## Resolved generic shared receiver reads (2026-11-22)

Methods with a shared struct receiver and scalar substitutions can lower
receiver field reads using the concrete instance layout. The receiver's
resolved definition must match its live slot, and only scalar field results
qualify. Owned and mutable receivers retain the typed body path.

## Resolved generic mutable receiver stores (2026-11-22)

Generic methods with scalar substitutions can read scalar fields through
`&mut self` and store to a directly selected scalar field. The resolver's
write occurrence must target the receiver slot. The typed path still
handles owned receivers, nested stores, and non-scalar field writes.

## Resolved generic owned receiver reads (2026-11-22)

Generic methods taking a struct receiver by value can lower scalar field
reads. The receiver stays in the parameter cleanup scope, so early returns
carry its drop and the existing drops pass owns cleanup. Whole-receiver
moves, receiver stores, and non-scalar field results retain the typed path.

## Resolved generic receiver returns (2026-11-22)

An owned generic struct receiver can be returned directly or through a
parenthesized expression when the return type is the same instance. The
body route records the movement and preserves separate branch move states
until the join. The existing drops pass handles the returned slot.

## Resolved generic receiver call arguments (2026-11-22)

An owned generic struct receiver may move into a direct function call on
the resolved body route. The argument preserves the original slot and
move check, and later receiver reads retain the moved-value diagnostic.

## Resolved closure capture probes (2026-11-22)

Capture probes use the same resolved body entry as emission when their
candidate parameter types and binding identities qualify. Both probes still
run in their original order, and the existing AST capture walk determines
which hidden parameters are used, moved, or mutated.

## R3d completion visibility (2026-11-22)

Completion uses the resolver's binding visibility ranges for parameters,
initialized locals, loop bindings, match payloads and closure parameters.
Sibling scopes can reuse a name without leaking its previous definition.
Repeated requests for the same document version reuse the cached resolution.

## R3d closure parameter shadowing (2026-11-22)

Closure parameters can shadow enclosing bindings, matching the compiler.
Resolver lookup restores the outer definition on closure exit. Completion
shows only the most recent binding visible at the cursor, and definition
jumps distinguish the inner and outer names. Duplicate ordinary locals
still produce E0104.

## R3c shared type rules (2026-11-22)

[ADR-0231](../adr/0231-shared-type-checking.md) governs the shared type rules.
The resolver and compiler now use the same operator checks for `i64`, `f64`,
and `bool`. Resolver inference covers integer bitwise operators and withholds
types for rejected scalar expressions. The compiler retains its diagnostic
positions and string lowering.

Resolved function bodies now record checked scalar types, rejected operators,
and deferred expressions by full byte range. The walk checks children before
their parents and carries local scalar types by definition identity. Rejection
stays at the operator that failed; enclosing operators defer. The compiler
still owns user-facing operator diagnostics. Eligible compiler bodies use
checked and rejected scalar facts after lowering operands, with deferred
operators checked against the compiler's concrete types.

Fixed-array reads and writes share the integer index rule across both parser
routes. The resolved route uses checked index types when available and retains
the compiler's E0201 location.

Plain and compound local assignments share compatibility checks. The resolved
route uses checked RHS types by byte range, and the compiler handles string
concatenation and ownership. Both routes also share the return type check,
using resolved scalar facts when available and the active compiler signature
for concrete types. Direct by-value call arguments also share the type check.
The resolved route reads scalar facts by argument range and uses the selected
compiler signature for concrete types. Borrowed call arguments share
compatibility and address selection based on concrete array lengths, vector
kinds, and struct instance IDs. Confirmed and speculative lowering still
record their effects at their respective commit points.

Struct field initializers and stores share the same type diagnostic. A
resolved scalar constructor now reports a mismatch at the initializer
boundary, where the token route would report it. Eligible constructors on both
routes validate field names and duplicates in source order, then report
missing fields in layout order.

Enum constructors share variant and payload lookup on the concrete enum
layout. Shared lowering uses checked scalar payload facts and concrete
compiler types for other payloads. On rejection, the token route retains
the payload ordinal, source location and diagnostic ordering. Explicit enum
specializations can also use shared lowering when their concrete layouts
are already cached. Cache misses leave instance allocation and diagnostics
to the committed token route. Cached struct specializations now use the
same lookup rules and share field checks while lowering initializers in
source order and emitting fields in layout order.

Explicit generic calls can reuse a cached function signature
through shared lowering. Cache misses stay on the committed route, which
instantiates the body and checks constraints. Shared lowering validates
arguments against the cached concrete signature and commits ownership
effects only after the call is accepted. For imports, the concrete registry
must identify a public template and local resolution must leave the call
unbound. A cached private template still falls through to the canonical
visibility diagnostic.

At the committed specialization site, pure scalar, string and vector
arguments can be read directly from resolved syntax ranges on first use.
The existing instantiator still owns allocation order, bounds checks and
instance identities. Unsupported or malformed type arguments use the
typed reader and its diagnostics.

The C backend uses the concrete vector type in forward declarations and
definitions. The owned-return regression covers a generic identity function
instantiated with `vec[f64]`, whose returned buffer remains live in the caller.

After first-use generic function instantiation, the committed call site can
lower its indexed argument list against the concrete signature. The shared
argument planner commits ownership changes only after the complete list
passes. Unsupported syntax and rejected arguments are parsed by the typed
argument reader at the original cursor.

First-use generic struct construction now shares initializer lowering
after the concrete layout is allocated. The initializer plan preserves
source evaluation order and layout field order, and commits moves only
after all fields pass. The original constructor still reports malformed
fields.

First-use enum constructors use the indexed argument list after variant
and visibility checks. They share payload lowering with cached enum
constructors, while the typed reader retains diagnostics for rejected
lists.

## Frontend cutover audit at 8aeeae0

| Area | Current route and remaining coverage |
| --- | --- |
| Function bodies | `scalar_body_statement` admits scalar bindings, assignments, returns, print/assert, if/while, range loops, unsafe blocks and loop control. Pointer stores, iterable loops, matches and effect statements still enter the token statement driver. |
| Expression semantics | The shared expression path handles eligible reads, operators, calls, casts and aggregates within either body driver. Receiver dispatch, closure capture discovery and complex ownership cases still rely on the compiler's context-sensitive handlers. |
| Explicit specialization | First use lowers indexed arguments or initializers after concrete instance allocation. Cached instances can lower speculatively. Rejected and unsupported shapes still rely on the token diagnostics. |
| Concurrency | Task, scope and pool spawns share an indexed handle operand. The callable still enters the expression parser, which may consume indexed closure headers and bodies. Capture validation and transfer happen after parsing the whole statement. |
| Verification | The A/B gate compares emitted C and CLI behavior. It does not establish which driver handled each input. The compiler's targeted parser test calls the resolved entry point and supplies that evidence for eligible shapes. |

Keep R3c and M0.2.08 open. Spawn callables now use the indexed operand for
thread, task, scope and pool spawns. A complete indexed inline closure enters
the existing capture analysis directly. Bound closures can use the
indexed local read. Both routes still apply the same signature and capture
checks at the spawn site. Other callable expressions retain the Pratt path.

Resolved bodies can store through a bound raw pointer with a scalar
pointee. Both body routes use the same mutability, unsafe-context and
value-type checks. Pointer-producing expressions still determine body
eligibility independently.

Zero-argument `deref()` on a resolved scalar pointer binding now lowers
directly in eligible bodies. The typed and resolved routes share the
unsafe-context check at the method name.

Eligible resolved bodies take the address of a local scalar binding by
definition ID. Address construction shares the token route's mutability,
pointer qualifier and pointee checks. A resolved body can now construct,
write through and read back a scalar local pointer in the same function.

Resolved bodies now lower for-in loops over borrowed integer and floating
point vectors. Both body drivers share vector iteration, including
container borrows, hidden slots, element access and move checks. Token
cursors retain the typed path.

Owning numeric vectors can now be built and iterated in resolved bodies.
Numeric literal bindings are tracked even when empty or computed, and
temporary loop borrows end before the owning binding is released. Ordinary
ownership transfers still use the typed path.

String vector parameters can now use the resolved vector loop path.
Each element remains a read-only string view, and binding a borrowed
container or a mutable view produces the same diagnostics in both drivers.
Local string vectors produced by `strvec_new`, `split` or `lines` now join
the same resolved loop route. Construction keeps the builtin argument
checks, and the local vector releases after its loop views expire.

Resolved bodies handle local token cursors and for-in iteration. Both
drivers share the loop setup and bind each token as a read-only view.
Cursor construction reuses the existing tokenize checks and registers
its base borrow at the declaring scope.

Eligible matches on local enum parameters use resolved arm bodies and
associate payload bindings with concrete compiler slots. The two drivers
share variant, arity and exhaustiveness checks as well as the enum borrow.
Locally constructed enum subjects now enter the same route. Construction
uses the resolved variant occurrence and the compiler's concrete layout.
Ordinary enum transfers retain the existing ownership path.

Resolved bodies now lower scalar `exit` statements. Both body drivers
validate the status at the closing parenthesis and attach the same live
owned values for cleanup.

Bare direct-call propagation can now be lowered from a resolved body.
Both drivers use the same result-shape and enclosing-return checks. Enum
constructor returns still use the existing ownership-aware operand path.
Propagation that binds the success payload landed too: `let value =
step()?;` joins the body route's Binding arm (the Propagate(Call)
initializer lowers through lower_shared_value's boundary unwrap, and
propagate_statement binds the payload with the canonical checks).
R3d's textDocument/documentSymbol, textDocument/references and
workspace/symbol all read the def table now: the outline lists
file-local functions/structs/enums/traits with their declared name
ranges (capability advertised in initialize), references returns
scope-exact occurrences (includeDeclaration honored),
workspace/symbol scans every open document's defs case-insensitively,
and rename crosses imported files (the import module's declaration and
internal uses edit from a fresh per-module resolution alongside the
entry's occurrences).


## Closure bodies landed (2026-11-22 09:23, `895ee85`)

The first closure rung is in: `lower_resolved_body_with_captures` lowers
eligible closure bodies from the resolver's `ClosureBody` trees (the
resolver records capture starts per closure and flags duplicate-binding
conflicts), with the closures driver trying the resolved route before the
typed walker. The guard accepts scalar/aggregate/enum params and capture
forms; generic substitutions stay deferred. The next rungs widen the
capture-type guard (String/Map captures carry ownership effects) — see
the blueprint below.

## Next arc blueprint: no-capture closures through the body route

The ladder's final rung (generic and closure bodies) starts with the
simplest closure shape. The verified machinery map (2026-11-22 16:00):

1. `closure_literal` (closures.rs:82) runs a two-probe capture inference:
   probe 1 binds every enclosing name in borrow form, probe 2 (only if
   probe 1's body parse fails) binds everything owned; `collect_capture_uses`
   reads used/mutated from the probe's leading params. The FINAL body parse
   re-parses the tokens with the chosen capture set.
2. `parse_closure_body` (closures.rs:232) parses the body tokens through the
   typed statement walker with captures + declared params bound as leading
   parameters. Its `Function` feeds the generics.closures registry.
3. The resolver already retains closure statement trees
   (`FnOccurrences.closure_bodies`) and closure params are
   `DefKind::ClosureParameter` defs — the def table covers closure bodies.

First slice: **closures with zero captures**. For those the probe system
concludes no captures, and the final body parse can lower through the
resolved route instead of the typed walker when the body passes
`scalar_body_statement` and the params/return are scalar. The route's
slots need the closure params bound — `scalar_definitions` carries the
ClosureParameter defs (keyed by name range), so `lower_resolved_scalar_body`'s
slots construction works if the closure params are registered in `names`
with their slots before the body lowers (parse_closure_body already binds
them for the typed parse).

Verification questions to answer while implementing:
- Does the resolver's closure walk register closure params into
  `scalar_definitions` (the collection's `is_local()` filter includes
  ClosureParameter ✓), and do their ranges match the header parse?
- The probe parses run BEFORE the final parse; the resolved route must not
  disturb the probe's `names` state (the probes run inside
  `closure_literal` on the same parser).
- The eligibility and lowering reuse the existing Binding/Return/... arms —
  no new arms, only a new driver entry in parse_closure_body's final parse.

Gates as always; a corpus closure example pins byte parity.

**Probed blocker (2026-11-22 15:40):** string captures are blocked in the
ELIGIBILITY, not the guard. A text binding's def sits in
`owned_text_bindings`, and the eligibility's Ident arm excludes reads
targeting it — including the reads INSIDE a closure that captures the
string (the resolver's closure walk resolves those to the outer binding's
def, since hidden capture params are compiler-side). Widening the route's
type guard alone changes nothing. The fix needs the resolver's
ClosureBody to record the captured outer defs, and the eligibility's
Ident arm to re-admit reads whose target is a capture of the enclosing
closure (offset-scoped to that closure's body range — ClosureBody needs
an `end` field too). The lowering side needs no change: a StrView
capture read lowers as a plain Variable through the capture slot.

The resolved closure route now accepts reads of captured string literals
when the compiler binds the capture slot as a read-only string view.
Resolution records outer local definitions and scopes the read to the
closure body. The owning capture form still uses the typed path.

The resolved closure route also lowers indexed reads of captured numeric
vectors through the concrete read-only capture slots. Integer and float
elements use the same vector-index expressions as the typed parser.
Owning and string-vector captures still use the typed path.

## M6.2 checklist closed (2026-11-22 20:00)

All seven M6.2 items are [x] in the ledger with per-item evidence (the
checklist is regenerated from it, R5.1). The LSP now has fourteen
def-table features (hover, definition across imports, scope-exact and
cross-file rename, scope-exact completion with receiver-typed member
completion, documentSymbol, references, workspace/symbol, signatureHelp,
foldingRange, codeAction, inlayHint, semanticTokens, documentHighlight,
selectionRange) plus compiler-sourced diagnostics — ADR-0224's D4
("LSP finds results by matching text") is closed. Latency on a
102,004-line project: warm completion 1.5 ms (the ignored probe pins
< 100 ms); cold is the one-time parse (~175 ms).

## M6.2 checklist adjudication note (2026-11-22 19:30)

M6.2.03 asks for "hover a shadowed name and show its local type and
declaration". AE does not allow same-name shadowing in an inner block —
the compiler reports `duplicate variable` (E0104) and the resolver's
diagnostics agree verbatim (pinned by
`lsp_shadowing_is_rejected_like_the_compiler`). The checklist item's
shadow clause is therefore not applicable to this language; its
substantive content (hover shows the local type and declaration) is
covered by the def-table hover and `lsp_hover_shows_binding_type_from_the_def_table`.

M6.2.01 (sequential unsaved edits, multibyte included) is pinned by
`lsp_sequential_edits_match_a_fresh_parse`: three successive revisions
answer identically to a fresh parse. M6.2.07 (cancelled queries) is
pinned by `lsp_cancelled_completion_does_not_mask_new_results`: the
cancel error is -32800 and the next request answers from the new buffer.

M6.2.05 ("complete inside a partially written member access using
receiver type") landed as receiver-typed completion: the identifier
before the dot resolves through the occurrence table to its inferred
struct, and that struct's declared fields answer (with a declaration-
driven fallback for incomplete `p.` text that does not parse).

## P1.1 next slice blueprint: generalize field access beyond `self`

Baseline (2026-11-23 01:00): `AE_ROUTE_REPORT=1` reports **1,186 legacy
fallbacks across 163 of 171 corpus files**. The two largest causes:

1. **Owned struct returns** (`fn io_submit_read(...) -> IoReq`): the guard
   admits a `Type::Struct(_)` return only when it equals the method
   receiver. A plain function returning a constructed struct needs the
   ownership path (the value is built in the body and moved to the
   caller) — a slice of its own.
2. **Field access on non-`self` roots.** `supports_field` (parser.rs
   ~1225) answers only for an impl method's `self`; `receiver_field_root`
   (parser.rs:70) matches the literal spelling `self`, and `receiver_value`
   (parser.rs:79) the same. So `fn io_step(r: &mut IoReq)` falls back at
   its first `r.state` read even though the guard now admits the `&mut`
   parameter.

The generalization, in one slice:

- Replace `receiver_field_root(expression) -> Option<usize>` with
  `field_chain_root(expression) -> Option<&Expression>` that returns the
  **root identifier node** of any field chain (walk `Field { base }` until
  an `Atom(Ident)`). Keep the old behaviour as a caller-side
  `is_self` check where the impl-receiver rule must stay (line 1133's
  `function.impl_owner.is_some()` gate).
- `receiver_field_type(expression, receiver: Type, structs)` already takes
  the receiver type as a parameter: it needs no change, only a different
  caller-supplied type.
- In `lower_resolved_body_with_captures`, **build `slots` before**
  `supports_field` (it currently builds after), then answer:
  root ident offset -> `scalar_reads`/`scalar_writes` -> def -> `slots` ->
  `(slot, ty)`; `supports_field` succeeds when
  `field_chain_type(expression, ty, structs)` is scalar (field reads and
  stores), or when the expression is the bare root and `ty` is
  `Type::Struct(_)` (a whole-value receiver read).
- Update the four sibling gates that consult `receiver_value` /
  `receiver_field_root` (parser.rs lines 181, 222, 331, 1133) to the same
  root rule, keeping the impl-receiver restriction only where the typed
  path requires it.
- In the lowering arms, `Field` and `FieldStore` already look the slot up
  by `scalar_reads`/`scalar_writes` def; verify they no longer assume
  `self` (they read `self.receiver` in a few places).
- Gate: `fake_io.ae`'s 24 fallbacks (offset map: `io_submit_read`,
  `io_submit_write`, `io_step`, `io_cancel`, ...) drop to only the
  owned-struct-return functions; `AE_ROUTE_REPORT` total falls several
  hundred; tests stay 510/0 and `ab_gate.py` stays byte-identical.

Owned struct returns are then the follow-up slice: the body constructs
`StructLit`, the return moves it, and `Frees` must not double-release it —
mirror what the closure-capture slices did for vectors.

## P1.1 slice 2 landed (2026-11-23): field access is no longer `self`-only

The blueprint above, as implemented:

- `field_owner_struct` takes `&ResolvedExpressions` and resolves a bare base
  through `scalar_reads` first, then `scalar_writes` (a store occurrence is
  keyed by the base offset, so `r.n = v` names `r` through the write map).
- The collection pass marks scalar store targets in `struct_field_writes`
  when the base is a bare ident in `scalar_writes` with a
  `Borrow { mutable: true }` spelling. Nested bases stay typed on purpose:
  the lowering indexes `scalar_writes[&base.range.start]`, so only a
  bare-base store is provable there today.
- `scalar_body_statement`'s field-store arm accepts any bare root and defers
  to `supports_field`; the per-function eligibility scan answers by the new
  marks (plus the impl `self` clause).
- `receiver_field_type` accepts any ident root; the lowering's
  `supports_field` already answered by slot type.
- `scripts/route_report.py` is the reproducible arbiter (`--details`,
  `--max`), wired into CI as a ratchet at 1184.

Measured: the fake_io program drops 24 → 16 fallbacks (the rest are the
owned-`IoReq` returns and the closure bodies), corpus 1191 → 1184 over the
same 165 files. `tests/run/mut_param_fields.ae` pins a `&mut` parameter's
reads and stores end-to-end; `tests/ui/shared_borrow_field_write.ae` pins
the shared-borrow rejection. Gates: 510/0, ab_gate 0 failed, std layering
holds.

Next slice: owned struct returns (`fn io_submit_read(...) -> IoReq` builds
and moves the struct out of the body), then the `pipe_*` functions follow.

## P1.1 slice 3 landed: owned plain-struct returns

`owned_plain_struct(ty, structs)` — no `Drop` impl, scalar fields only — is
the shape the body route can own outright. On that shape:

- `supports_call` admits a struct-returning callee (the construction
  allocates nothing a release routine would walk), so `return make(...);`
  lowers through the resolved route.
- The guard admits a `Type::Struct` return type (the receiver-equality rule
  for `return self;` stays).
- Bindings of call results are ordinary `Declare`s whose owned type enters
  the scope's Frees list, exactly like a drop-free construction.
- `return binding;` moves the slot out: the lowering marks it moved (drops
  pass trims its Frees), while `s = take(s)` still fails with the typed
  route's E0102, its position, and its hint (the resolved Assign arm now
  carries the same same-statement move check and re-initialization clear).
- New set `owned_struct_bindings` (spelled `Named`, not borrowed) admits the
  `return binding;` statement at scan time.

Coverage: 1184 → **987** fallbacks (the `batch_core_*` families dropped most:
semver/mvs/version-request helpers and friends). `tests/run/owned_struct_return.ae`
pins construction, `let` binding, and `return binding;` end to end.

Still typed: structs with owned fields (`IoReq`, `BytePipe` — construction
moves into a `string` field are not tracked), by-value struct parameters
(their exit drop is the typed path's), field reads on temporaries
(`f().field`), builtin calls in scalar bodies (`len`/`slice`/`+`), and
closure bodies' field receivers. The next slice with the best coverage per
risk is builtins plus string-field reads inside scalar bodies — that is what
`io_step`, `pipe_*` and much of `batch_core_*` still need.

## P1.1 slice 4 landed: by-value plain-struct parameters + literal prints

- The resolver spells a by-value struct parameter `Named`; the mark pass now
  registers those (not just `Named` bindings), so the field-read walk and
  `struct_definitions` see them like `&P` receivers.
- The guard admits a by-value parameter of the plain shape
  (`owned_plain_struct`): it is owned by the callee but has no release
  surface, so no exit drop is owed — reads, constructions from its fields,
  and moving it back out (`return s;`, passing it on) are the whole surface.
- A literal `print` lowers as `Printed::Text`, exactly the statement the
  typed route emits for it (byte parity).
- `AE_ROUTE_REASONS=1` (new) prints the first statement keeping a function
  or closure body off the route, with file:line and source text — the
  planning aid behind slices 2-5.

Coverage: 987 → **471 fallbacks**, files with fallbacks 165 → 127. The
incremental-compiler family went to zero (`ir_query_key`, `ir_dep_graph`,
`ir_crate_interface`, `ir_query_stats` all route now).
`tests/run/struct_value_params.ae` pins parameter reads, a construction from
parameter fields, `return s;`, and argument-position constructions.

The reason report for `examples/closures.ae` now names the next theme:
string expressions (`"hello " + name`, `to_string(...)`) and closure-valued
bindings. After that: structs with owned fields (`IoReq`/`BytePipe`),
closure bodies' field receivers, trait-method calls, nested store bases.

## P1.1 slice 5 landed: string expressions and pure value builtins

- A text literal is a scalar body operand, so `return note + "!";` and
  `print("[" + text + "]")` route.
- A string `+` lowers through `lower_infix` (the typed route's own function);
  the resolved Binary arm marks both owned operands moved, matching the
  typed postfix layer's consume rule.
- `lower_resolved_builtin` lowers `len`, `to_string`, `to_f64`, `trim`,
  `slice`, `split`, `contains`/`starts_with`/`ends_with` and every math
  function natively: same `Expr` variants, same messages, same positions.
  The arity branches reproduce the pinned parser diagnostics
  (`expected expression` E0011, `expected RParen, found Comma` E0010, and the
  math `takes N argument(s)`), and `len`'s variable fast path stamps the call
  position exactly like the typed route (a one-column parity diff caught it).
- A first attempt delegated whole builtin calls to the typed Pratt parser;
  it over-consumed (the typed entry lowers the *statement's* whole expression
  from the cursor), which produced wrong operand types and one miscompile.
  Reverted; the native arms are the fix.

Coverage: 471 → **465** fallbacks, files with fallbacks 127 → 126.
Parity stayed green only after the `len` position fix; the existing trim /
starts_with / math execution tests now run through the resolved route.

Next: borrow arguments (`Argument::View`/`Borrow`) — that is what keeps
every caller body typed (`print(label(a, 42))`), and after it string-typed
field views in concatenation (`task.title + " w="`).

## P1.1 slice 6 landed: string field views

A `string`-typed field is marked in `struct_field_views` (never a move). The
view is admissible exactly where the typed route borrows it:

- a `+` operand (`task.title + " w="`), where `lower_infix` emits the concat
  and no move is marked (the `Expr` is a field, not a variable);
- a `print` argument (`print(p.label)`);
- a call argument — a value builtin (`len(r.buf)`, `slice(r.buf, 0, r.pos)`)
  or a `&string` parameter.

`let s = p.buf;` and `return p.buf;` remain typed: those would move a string
out of a borrowed struct.

Coverage: 465 → **461** fallbacks. `examples/closures.ae`'s captured-struct
concat now routes, and `io_step` / `io_transferred` / `pipe_recv` in the
fake_io program do too.

What remains in fake_io is the owned-field struct slice: constructing
`IoReq { buf: src, .. }`/`BytePipe { buf: "", .. }` (a string moves into a
field) and storing an owned string through a field (`p.buf = p.buf +
slice(...)`), plus borrow arguments in callers.

## P1.1 slice 7 landed: owned strings and owned-field structs

- A `string` parameter is owned by the callee. The guard admits it and
  `live_owned` now carries parameters into every `return`, deduplicated
  against the entry scope (the receiver lives there already), so a body
  releases the parameters it did not move out. A closure's owned *string
  capture* stays typed — reading it would move the capture.
- `owned_field_struct`: a struct whose fields are scalar-or-`string`, no
  `Drop` impl. Constructions of that shape are marked in
  `owned_constructions`; string field values (literals, moved owned strings
  and parameters, string field views, concats, `to_string` results) move in,
  and the generated `ae_tN_release` frees them later. `make(...) -> Req`,
  `return Req { buf: src, .. }`, and `let r = make(...)` all route.
- `let other = owned;` moves the source (E0102 on later reads) — the general
  Binding path now records the move.
- `p.buf = p.buf + slice(...)` stores an owned string through a `&mut`
  receiver: `owned_field_writes` marks the target, and the existing
  FieldStore codegen releases the old value and consumes the right-hand
  side.

Coverage: 461 → **435** fallbacks. In the fake_io program every std function
now routes (io_submit_read/write, io_step, io_transferred, pipe_new/send/
recv/close, source_new/track/post, oracle_*); only the fixture's own `main`
remains typed because it passes `&mut r` (borrow arguments are the next
slice). `tests/run/owned_string_fields.ae` pins construction, field store,
length, and a string-returning function end to end.

Gates: 510/0 on Windows and WSL2, ab_gate 0 failed (byte parity held even
with the new parameter releases), sanitize+leak PASS, fuzz 0 failures.

Next: borrow arguments — `Argument::View`/`Borrow` on the resolved route.
That is what keeps every caller body typed.

## P1.1 slice 8 landed: `&string` arguments and concat move order

- A `&string` parameter is `Type::StrView` in the signature. `supports_call`
  admits it, and the resolved Call arm lowers that argument as
  `Argument::View` with the typed route's own check and message
  ("argument must be a string or string view, found ..."). A view never
  moves; owned string idents are admissible in argument position.
- A string `+` consumes its left operand *before* the right side is read:
  the resolved Binary arm marks the left `Expr::Variable(String)` moved
  before lowering the right, so `let t = s + s;` fails with E0102 on the
  second read exactly as the typed route says. Owned string idents are
  admissible as `+` operands for the same reason (the concat consumes them).

Coverage: 435 → **434**. Byte parity held (50 examples + 663 CLI
invocations), 510/0 on Windows and WSL2.

Still typed: `Argument::Borrow` for `&vec`/`&vec[string]`/`&struct`
parameters (the address-taking/forwarding and the call-borrow register),
closure-valued bindings, `match` on non-ident subjects, generic-instance
structs, for-in over borrowed vectors, trait-method calls.

## P1.1 slice 9 landed: borrow arguments

- A `&vec` / `&vec[string]` / `&vec[f64]` / `&Struct` / `&mut Struct`
  parameter joins `supports_call`. The resolved Call arm lowers `&x` /
  `&mut x` (the shared tree's `BorrowArgument` node) as `Argument::Borrow`
  with the typed route's own `CallBorrow::classify`, messages, positions and
  registers: `call_borrows` (statement-scoped, for `&Struct`), `mut_borrows`
  (for `&mut`), and the deferred view-conflict entry when only view borrows
  are active.
- The statement loop now clears `call_borrows`/`mut_borrows` at each
  statement, exactly like the typed block loop. That clear was the missing
  piece for two `&mut` calls in consecutive statements (owned_string_fields
  errored on the second one before it).
- `&x` handed to a by-value parameter reproduces the typed expression
  parser's "a borrow expression must be cast to a pointer".
- `mut_borrow_shared.ae`'s pinned message and position (14:33) still match.

Coverage: 434 → **428**, files with fallbacks 127 → 125. The fake_io and
owned_string_fields mains now route end to end (their `&mut` calls included).
Byte parity held (50 examples + 663 CLI invocations), 510/0 Windows/WSL2.

Next: closure-valued bindings and closure bodies over owned captures,
`match` on non-ident subjects, generic-instance structs (`Cell[T]`,
`ListNode[T]`), for-in over borrowed vectors, trait-method calls.

## P1.1 slice 10 landed: prelude/generic enums and owned-vector positions

- A `Qualified` constructor (`Option[i64]::None`, `Option[string]::Some(x)`)
  is marked as an enum constructor when its base names a known enum after
  peeling `T[...]`/bracket wrappers — `enum_names` plus `generics.names`
  (prelude generic enums). `return Option[...]::...;` routes.
- `enum_bindings` now recognises the resolver's third enum spelling,
  `Prelude { name, arguments }`, so `match line` over an `Option[string]`
  parameter routes (the arms' string bodies already did).
- Owned vectors join `len` arguments and indexing (`len(all[i])`): a builtin
  or `&vec` parameter only borrows the vector, and indexing copies/views an
  element.
- The route-reason report now descends into compound statements (if/while/
  for/match/unsafe bodies) and names the innermost failing statement, which
  is what made this slice's shapes visible.

Coverage: 428 → **416**, files with fallbacks 125 → 124. `match parsed`
(`let parsed = parse_int(...)`) still falls back: that binding's resolver
spelling is `None`, so the scan cannot prove it is enum-typed; the lowering
would know. Next: teach the scan the initializer's type, or route the
builtin call's known prelude return through a mark.

Next themes: closure-valued bindings and closure bodies over owned captures,
generic-instance structs (`Cell[T]`, `ListNode[T]`), for-in over borrowed
vectors, trait-method calls (`num_traits`, `default_trait`), nested store
bases.

## Tonight's P1.1 arc at a glance (2026-11-23, slices 1-10)

| slice | shape | commit | fallbacks |
|---|---|---|---|
| 1 | guard widening for `&mut` struct params | `b36b2c5` | — |
| 2 | field receivers from any routed slot (`&mut` writes) | `fd29ee7` | 1,191 → 1,184 |
| 3 | owned plain-struct returns | `ff4fc3f` | 1,184 → 987 |
| 4 | by-value plain-struct params, literal prints, reason report | `5264391` | 987 → 471 |
| 5 | string expressions + pure value builtins | `98439a2` | 471 → 465 |
| 6 | string field views (concat/print/args) | `6faf1ff` | 465 → 461 |
| 7 | string params + owned-field structs (construct/store/release) | `9786192` | 461 → 435 |
| 8 | `&string` args as View; concat consumes operands in order | `e506501` | 435 → 434 |
| 9 | borrow arguments (`&vec`/`&Struct`/`&mut Struct`) | `6ad2444` | 434 → 428 |
| 10 | prelude/generic enums, owned-vector args/index | `3e6623e` | 428 → 416 |

Total: **1,191 → 416 fallbacks** (65% of the legacy bodies moved), files
with fallbacks 165 → 124. Every slice landed with: `cargo test
--workspace --locked` 510/0 on Windows and WSL2, `ab_gate.py` byte parity
(50 examples + 663 CLI invocations), sanitize+leak PASS, fuzz 40 seeds,
std layering, commit-id discipline. `scripts/route_report.py --max 416` is
the CI ratchet (bump only downward).

Fleet: 866,374 seeds / 91 segments, zero failures; InstaBox certificates
were renewed 2026-11-23 00:45 (valid 24h).

## P1.1 slice 11 landed: enum-result builtins

- `parse_int`, `to_i64`, `find` and the `json_*` family lower natively:
  existing `Expr` variants, the typed route's argument checks/messages, and
  the prelude enum result types (`IntResult`, `ReadResult`, `BoolResult`,
  or plain `Bool` for `json_has`).
- `ENUM_RESULT_BUILTINS` marks a binding initialized by one of them as
  enum-typed even when the resolver leaves the spelling open (`let parsed =
  parse_int(...)`), so `match parsed` routes.

Coverage: 416 → **410**, files with fallbacks 124 → 123. `examples/maybe.ae`
is fully routed now; `json_report.ae` keeps one typed function whose match
subject is a `try_create_dir` result (an effectful builtin, deliberately
still typed).

Next: effectful `try_*` builtins (need the typed route's effect/result
handling), closure-valued bindings, generic-instance structs (`Cell[T]`),
for-in over borrowed vectors, trait-method calls, nested store bases.

## P1.1 slice 12 landed: the endgame switch

- `AE_DENY_LEGACY_ROUTE=1` makes every body the resolved route does not
  take a diagnostic (functions and closures): "the legacy body parser is
  disabled ... this function's body is not on the resolved route yet".
  `AE_ROUTE_REPORT` keeps printing the legacy lines first, so a corpus run
  still counts them.
- `scripts/deny_route.py` is the manifest of files that are demonstrably
  legacy-free and CI runs it every build. Current list:
  `tests/run/mut_param_fields.ae`, `tests/run/owned_string_fields.ae`,
  `tests/run/struct_value_params.ae`. Add a file once `route_report.py
  --details` shows no `route legacy` line for it; when the list covers the
  corpus, delete the legacy parser (P1.2-P1.4).
- `tests/execution.rs::deny_legacy_route_turns_fallbacks_into_diagnostics`
  pins both sides (a routed program compiles, a generic-instance
  construction fails with the message).

Gates: 511/0 on Windows and WSL2 (the new test), byte parity 0 failed,
sanitize+leak PASS, fuzz 0 failures, layering and commit-id discipline hold.
Fallback count unchanged at 410 — this slice is the switch, not a widening.

## P1.1 slice 13 landed: slice_view, borrowed arrays, enum-returning calls, string assignment

- `slice_view` lowers natively with the typed route's rules and messages: a
  string variable, or a string field of a struct *variable* (the view
  registers a `BorrowKind::View` on the whole variable with the current
  scope depth).
- Borrowed-array arguments join the call surface: a `&[i64; N]` parameter
  takes `&values` as `Argument::Borrow(slot, false)` (classify Forward).
- A user function returning a prelude enum joins `supports_call`
  (`Type::Enum(..)` returns), so `let a = upper(true); match a { ... }`
  routes.
- An owned string prints as a view, and may be reassigned: the resolved
  Assign arm now releases the old value (`Statement::Assign(..., ty.owned())`),
  moves an owned right-hand side in, and applies the typed view rules —
  a string's view conflict defers to the graph (so the pinned NLL message
  and its position, the borrowed *name*, still match), every other owned
  type rejects on sight.
- The route-reason report also reports bodies the lowering's own gates
  reject ("route reason blocked ..."), which found these last shapes.

Coverage: 410 → **397**, files with fallbacks 123 → 113. Five examples are
now legacy-free and joined the deny manifest: borrow, concat, field_view,
option_propagate, strings (manifest now 8 files, CI-gated).

Next: `push`/`pop` statement builtins (`tokens.ae`, `io/reader.ae`), map
stores (`counts[word] = ...`), effectful `try_*`/`read_file`, generic-instance
structs (`Cell[T]`), method calls on `self`, function-pointer calls.

## P1.1 slice 14 landed: vector statement builtins and join

- `push`/`pop`/`sort`/`reverse` lower natively to the existing statements
  (`VecPush`/`StrVecPush`/`VecFPush`, `*Pop`, `Sort`, `Reverse`) with the
  typed route's rules: moved target, vec-kind check, mutability and borrow
  rejection, element typing, f64 sort rejection, and the pinned diagnostic
  positions (just past the target name — the `,` or `)`).
- `join` lowers natively (`Expr::Join`), borrowing a bare `vec[string]`
  variable; other shapes keep the typed message.
- `tokenize`-fed loops (`for f in cursor { push(kept, f); }`) route as a
  whole now.

Coverage: 397 → **394**, files with fallbacks 113 → 110. Two more examples
are legacy-free and joined the deny manifest (tokens, heart — 10 files
total, CI-gated).

Next: map stores (`counts[word] = ...`), effectful `try_*`/`read_file`,
generic-instance structs (`Cell[T]`/`ListNode[T]`), method calls on `self`,
function-value calls, `task_yield`/`cell_load` in bodies.

## P1.1 slice 15 landed: maps

- `map_new`/`f64map_new`/`map_get`/`map_has`/`map_remove`/`map_inc`/`keys`
  lower natively: `MapPlace::Var` (a map field of a struct variable too),
  the prelude result enums (`IntResult`/`FloatResult`), the typed messages,
  and the `MapKind` element rules.
- A `keys(...)` binding is marked as an owned string vector (for-in works);
  a `map_get(...)` binding as enum-typed (match works); a `map_new` binding
  is marked as a map even when the resolver leaves its spelling `None`.
- `m[key] = v` lowers to `MapPut` with the typed checks: mutable target,
  borrow rejection, string key, element type.

Coverage: 394 → **391**, files with fallbacks 110 → 107. Three more examples
are legacy-free and joined the deny manifest (map_count, map_missing,
sensor_stats — 13 files total, CI-gated).

Next: effectful `try_*`/`read_file`/`env`, generic-instance structs
(`Cell[T]`/`ListNode[T]`), method calls on `self`, function-value calls,
concurrency builtins in bodies (`cell_load`, `task_yield`).
