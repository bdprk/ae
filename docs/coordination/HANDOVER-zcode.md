# Handover — zcode relay

- Date: 2026-11-25 04:30 +08:00
- Branch: `main` (direct relay commits; no `agent/*` branch or worktree — the session ran in the primary checkout)
- Worktree: `D:\用户\trevv\桌面\projects-bdprk-副号\ae`
- Area claim: none active (no other agent's uncommitted work was touched)
- Model/tool: ZCode (deepseek-v4.1-flash), Git Bash on Windows 11; Linux leg = WSL2 Ubuntu clone at `~/ae`

## What is done

**P1.4 closed — M0.2.08, R3a and R3c flipped (`28659dee`).**

- `27d630b` the legacy typed block parser is deleted; P1.1 closes.
- `1019a9b` `parser.rs` split into focused modules, every lowering file under 800 LOC.
- `40f0ad6` the typed-expression fallback becomes a measured ladder (257 fallbacks).
- `25ee1a0`, `7156629`, `b455283`, `431aac0`, `af367b4`, `7bc2c1d`, `579c13b`, `c991009` drive it 257 → 0 (enum/generic constructions, spawn handles, pointer intrinsics, associated calls, `tokenize`, bracket indexes, returned-receiver chains).
- `ab5ebb0b` the token-level expression parser itself is deleted (`parser.rs` 436 lines, `expression.rs` 411); one route: shared syntax tree → resolved marks → typed `Expr` → MIR.
- Evidence recorded in the ledger's P1.4 Exit: dual-platform workspace suite 532/0, A/B byte-parity 50 examples + 663 CLI invocations with zero diffs on Windows and WSL, WSL sanitize+leak 41/41 (leak 0), `diff_fuzz.py --seeds 10000` with 0 failures, deny 176/176, route report 0/0.

**P1.3 closed (`11a3d31`).** `crates/ae-hir` with `HirModule/Item/Stmt/Expr/Pat`, stable `HirId`s, `DefId`+scalar facts, parsed closure bodies and structured trait bodies, `ae emit-hir`, `tests/hir` snapshots; opaque-body list empty (`2d1d6d6`, `f808396`, `097ec94`, `01609a1`).

**P2.1 in progress.** `c38dd48` ADR-0232 + `spec/types.md` numeric semantics; `8197dd6` `IntTy`/`FloatTy` vocabulary; `132ba33` mechanical `TyKind::Int(IntTy)`/`Float(FloatTy)` migration (byte-identical, dual-platform verified via `8afb19f`); `7040504e` scalar `as` casts i64↔f64 (grammar `ScalarCast` → resolver inference → HIR → route gate → lowering → `Expr::ScalarCast` → C codegen with the trapping `ae_f64_to_i64_trunc` helper and mirrored interpreter), regression test pinned (`18f6e83` finalizes evidence).

**P1.2 partially done (still open).** `ae-ty` crate with operator rules, `emit-types` + snapshots, width-aware `TyKind`, per-compilation `TypeInterner`, canonical scalar bridge/value acceptance, `DefId`+`TypeSpelling` re-exports, and a tested `unify` API (not yet wired into the compiler). Entries `0f54986`, `85973dc`, `7937a24`, `8bb2c4d`, `2fac958`, `a63d7e2`, `6b8c414`.

## What is half-done

- **P2.1 widths**: only `i64`/`f64` are spellable (`scalar_type`); `i8/i16/i32/isize/u8/u16/u32/u64/usize/f32/char/()` are not implemented. `as` only covers i64↔f64. Arithmetic is still width-blind and has i64-only checked runtime helpers; no per-width boundary fixtures. P2.1 exit gate NOT met.
- P2.1 remaining pieces: literal typing/suffix decision, checked/wrapping/saturating methods per width, char literals/validity, unit type in signatures.
- **P1.2**: `ae_ty::unify` exists and is tested but has no caller; the resolver's `TypeSpelling` and the compiler's `TyKind` are still two representations; `emit-types` pins four fixtures.
- **P1.3**: the HIR is a borrowed view over `ae-syntax` nodes; owning cloned nodes and compiler consumption of HIR remain later slices.
- Phases P2.2–P8 are untouched.

## Next step

```sh
cd "D:\用户\trevv\桌面\projects-bdprk-副号\ae"
git status -sb
cargo test --workspace --locked
```

Then read `docs/adr/0232-primitive-scalar-widths.md` and the P2.1 ledger item, and start the i8/u8 width slice: extend `scalar_type` (`crates/ae-compiler/src/frontend/types.rs`), allow the cast target, add width-aware operator rules/checked runtime helpers (C `__builtin_*_overflow` + interpreter `checked_*`), and pin MIN/MAX/overflow fixtures in `tests/run`/`tests/ui` with compiled-vs-`--interp` parity. Keep the ratchets at `route_report.py --max 0 --max-typed 0` and regenerate the checklist after ledger edits.

## Evidence

- Windows `cargo test --workspace --locked`: **535 passed / 0 failed** at `7040504e`; clippy `--workspace --all-targets -- -D warnings` clean.
- WSL Ubuntu on the same tree: 535/0, 50k mutation gate green, A/B gate against `31bd1ae9` 50 + 663 invocations zero diffs.
- Windows A/B vs `8197dd6` (width migration): zero diffs.
- Dual-platform A/B vs `c991009c` (P1.4 deletion): zero diffs.
- WSL `verify_generated_c.py --sanitize --leak`: 41/41, zero leaked blocks.
- `diff_fuzz.py --seeds 10000`: 10000 seeds, 0 failures (5.55 seeds/s, 8 jobs).
- `deny_route.py` 176/176; `route_report.py`: fallbacks=0 typed=0 (176 checked, 15 expected standalone skips).
- CI: `28659dee`, `8afb19f9`, `31bd1ae` green; `7040504e` run `45301746117` was in progress at handover; `18f6e839` queued.
- Failure lessons: `agent-failure-log/2026-11-24.md` (local only — the directory is ignored by `.git/info/exclude`).

## Risks and blockers

- No hard blocker. CI for `7040504e`/`18f6e839` must be checked; fix forward on failure (never force-push/rewrite published history).
- Auto-push post-commit hook races manual pushes (`cannot lock ref … expected …` or `Everything up-to-date`); verify with `git fetch origin && git rev-parse origin/main`.
- The ledger is CRLF in the index: edit it with bytes (`open(..., "rb"/"wb")`) or a one-line change becomes a whole-file diff (happened at `888d05f`, repaired at `f9ea6e2`).
- `wsl_sync.py` aborts at `check_commit_ids.py` with a dirty tree; manually `git fetch origin && git reset --hard origin/main` in `~/ae` and `git apply` the patch. Never reset the WSL clone while a fuzz run is using it.
- `agent-failure-log/` is locally ignored by design; publish only if the user asks (`git add -f`).
- Commit titles naming ledger IDs must edit `docs/implementation-ledger.md` in the same commit (`scripts/check_commit_ids.py`).
- The primary checkout is on `main`; with multiple agents, create the `agent/<name>` worktree per `AGENTS.md` before editing.

## Files touched (major)

- `crates/ae-compiler/src/frontend/`: `parser.rs`, `expression.rs`, `declarations.rs`, `types.rs`, `closures.rs`, `mir.rs`, `codegen.rs`, `interp.rs`, `mod.rs`, `route_*` and `lower_*` modules, `runtime_ex.c`
- `crates/ae-ty/`, `crates/ae-hir/`, `crates/ae-resolve/` (resolver walk/inference), `crates/ae-syntax/src/expression.rs`
- `tests/execution.rs`, `tests/hir/`, `tests/types/`, `tests/intrinsic_freeze.rs`, `tests/ui/`
- `docs/implementation-ledger.md`, `docs/checklist.md`, `docs/adr/0232-primitive-scalar-widths.md`, `docs/platform-matrix.md`, `spec/types.md`, `AGENT_PROGRESS.md`
- `scripts/route_report.py`, `scripts/deny_route.py`, `.github/workflows/test.yml` (typed-fallback ratchet)