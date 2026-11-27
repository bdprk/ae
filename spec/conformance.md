# Frozen observable behavior (v0.1.1)

Status: frozen at 0.1.1. Every row is a public contract that a case in the
default `cargo test --workspace` run pins; a release re-verifies this matrix
and bumps the version in the heading, which the conformance gate enforces.
Rows cover the contracts around the compiler — the command line, diagnostics,
runtime traps, backend agreement, editions/migration, packages and the editor
protocol. Language semantics are pinned by `spec/*.md` and the `tests/run`
corpus (`B1`).

## 1. Command line

| ID | Frozen behavior | Case |
|---|---|---|
| C1 | A usage error (unknown command, bad flag, wrong arity) exits 2 and prints usage on stderr. | `tests/execution.rs::cli_exit_codes_distinguish_usage_compile_and_program_failures` |
| C2 | A compile error exits 1, prints `ae: line:col: message` on stderr, and writes no stdout. | `tests/execution.rs::cli_exit_codes_distinguish_usage_compile_and_program_failures` |
| C3 | `ae run` returns the program's own exit status, including `exit(n)`. | `tests/execution.rs::cli_exit_codes_distinguish_usage_compile_and_program_failures` |
| C4 | `ae test` reports passing, failing and trapped tests with distinct exit codes. | `tests/execution.rs::test_runner_reports_pass_fail_and_panic_with_exit_codes` |

## 2. Diagnostics

| ID | Frozen behavior | Case |
|---|---|---|
| D1 | Every diagnostic code the compiler or lexer assigns has text in the `ae explain` catalog. | `crates/ae-compiler/src/frontend/mod.rs::every_assigned_code_has_an_explanation` |
| D2 | An `unsafe` block with no unchecked operation compiles and warns `W0702`. | `tests/execution.rs::unnecessary_unsafe_block_warns_without_failing_the_compile` |
| D3 | `strvec_new()` compiles in edition 2 and warns `W0703` with the replacement and the removal policy. | `tests/execution.rs::deprecated_strvec_new_warns_with_replacement_and_removal_policy` |

## 3. Runtime traps

A trap prints `ae: line:col: message` on stderr and exits 1; the expected text
and status are the fixture's saved `.stderr`/`.exit` baseline.

| ID | Frozen behavior | Case |
|---|---|---|
| T1 | Integer division by zero traps with `division by zero`. | `tests/run/divide_by_zero.ae` |
| T2 | An out-of-range vector index traps with `vector index out of bounds`. | `tests/run/vector_out_of_bounds.ae` |
| T3 | A failed allocation traps with `out of memory`. | `tests/run/extern_alloc_oom.ae` |
| T4 | A spawned body's trap surfaces at `wait`, at the waiting program point. | `tests/run/thread_failure.ae` |

## 4. Backends

| ID | Frozen behavior | Case |
|---|---|---|
| B1 | Every accepted `tests/run` fixture compiles through the pure-MIR C11 pipeline and matches its saved stdout (and stderr/exit for traps). | `tests/pure_e2e_corpus_gate.rs::pure_mir_c11_pipeline_matches_fixture_output` |
| B2 | The debug C, release C and interpreter paths agree on a representative corpus. | `tests/execution.rs::three_way_differential_agreement_between_debug_release_and_interp` |
| B3 | A container fixture produces identical output on the compiled, MIR, tree and interpreted paths. | `tests/execution.rs::slice_mut_match_across_backends` |

## 5. Editions and migration

| ID | Frozen behavior | Case |
|---|---|---|
| E1 | A new-edition package consumes an old-edition package; only the new-edition call sites warn. | `tests/execution.rs::an_old_edition_package_is_consumed_by_a_new_edition_package` |
| E2 | `ae migrate` rewrites the deprecated alias and a second run changes nothing. | `tests/execution.rs::migrate_rewrites_the_deprecated_alias_and_a_second_run_changes_nothing` |

## 6. Packages

| ID | Frozen behavior | Case |
|---|---|---|
| P1 | A verified cached archive extracts and builds offline. | `tests/package_integrity_m3307.rs::valid_cached_archive_verifies_extracts_and_builds_offline` |
| P2 | A one-byte tampered archive fails closed before extraction or compilation. | `tests/package_integrity_m3307.rs::tampered_one_byte_archive_fails_closed_before_extraction_or_compilation` |
| P3 | The vendored downstream corpus rebuilds under its committed locks and matches its baselines. | `tests/execution.rs::pinned_downstream_packages_rebuild_and_match_baselines` |

## 7. Editor protocol

| ID | Frozen behavior | Case |
|---|---|---|
| L1 | `initialize` advertises the server capabilities the editor relies on. | `tests/lsp_server.rs::lsp_initialize_and_capabilities` |
| L2 | Rename edits a symbol across files. | `tests/lsp_server.rs::lsp_rename_symbol_across_file` |
