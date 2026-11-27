#!/usr/bin/env python3
"""The P1.1 deny manifest, kept as the P1.4 deletion gate: files that must
compile now that the typed body parser is gone.

Every file listed here has no legacy body left — the resolved route covers
its functions and closures completely. A failure means a body the route
does not take, which is a diagnostic in every mode since P1.4 removed the
typed fallback and its escape hatch.
"""

import argparse
import os
import subprocess
import sys

# Sorted so the manifest reads like a checklist; add new entries in place.
FILES = [
    "examples/arithmetic.ae",
    "examples/arrays.ae",
    "examples/borrow.ae",
    "examples/closures.ae",
    "examples/concat.ae",
    "examples/constraints.ae",
    "examples/control-flow.ae",
    "examples/copy_notes.ae",
    "examples/dir_report.ae",
    "examples/enum_methods.ae",
    "examples/enums.ae",
    "examples/field_view.ae",
    "examples/file_tidy.ae",
    "examples/find_files.ae",
    "examples/find_text.ae",
    "examples/float_math.ae",
    "examples/float_stats.ae",
    "examples/function_types.ae",
    "examples/functions.ae",
    "examples/generic_borrow.ae",
    "examples/generic_constraints.ae",
    "examples/generic_fn.ae",
    "examples/generic_impl.ae",
    "examples/grep.ae",
    "examples/grep_cli.ae",
    "examples/heart.ae",
    "examples/hello.ae",
    "examples/json_report.ae",
    "examples/loops.ae",
    "examples/map_count.ae",
    "examples/map_missing.ae",
    "examples/math_basics.ae",
    "examples/maybe.ae",
    "examples/methods.ae",
    "examples/modules/main.ae",
    "examples/option_propagate.ae",
    "examples/pairs.ae",
    "examples/pipe_stats.ae",
    "examples/run_tool.ae",
    "examples/sensor_stats.ae",
    "examples/split_arena.ae",
    "examples/strings.ae",
    "examples/struct_borrow.ae",
    "examples/struct_mut_borrow.ae",
    "examples/sum_numbers.ae",
    "examples/text_preview.ae",
    "examples/tokens.ae",
    "examples/traits.ae",
    "examples/vectors.ae",
    "examples/word_count.ae",
    "tests/run/arithmetic.ae",
    "tests/run/batch_core_advanced.ae",
    "tests/run/batch_core_alloc_and_arith.ae",
    "tests/run/batch_core_compiler_abi2.ae",
    "tests/run/batch_core_compiler_asm.ae",
    "tests/run/batch_core_compiler_asm2.ae",
    "tests/run/batch_core_compiler_cfg2.ae",
    "tests/run/batch_core_compiler_codegen.ae",
    "tests/run/batch_core_compiler_coff.ae",
    "tests/run/batch_core_compiler_debug.ae",
    "tests/run/batch_core_compiler_dwarf2.ae",
    "tests/run/batch_core_compiler_elf.ae",
    "tests/run/batch_core_compiler_elf2.ae",
    "tests/run/batch_core_compiler_emit.ae",
    "tests/run/batch_core_compiler_frontend.ae",
    "tests/run/batch_core_compiler_ir.ae",
    "tests/run/batch_core_compiler_linker.ae",
    "tests/run/batch_core_compiler_macho.ae",
    "tests/run/batch_core_compiler_opt.ae",
    "tests/run/batch_core_compiler_opt2.ae",
    "tests/run/batch_core_compiler_opt3.ae",
    "tests/run/batch_core_compiler_pe2.ae",
    "tests/run/batch_core_compiler_primitives.ae",
    "tests/run/batch_core_compiler_regalloc2.ae",
    "tests/run/batch_core_compiler_runtime.ae",
    "tests/run/batch_core_compiler_runtime2.ae",
    "tests/run/batch_core_compiler_wasm.ae",
    "tests/run/batch_core_compiler_wasm2.ae",
    "tests/run/batch_core_compiler_wasm3.ae",
    "tests/run/batch_core_compiler_wasm4.ae",
    "tests/run/batch_core_encoding.ae",
    "tests/run/batch_core_generators.ae",
    "tests/run/batch_core_hash_table.ae",
    "tests/run/batch_core_hashing_fmt.ae",
    "tests/run/batch_core_heap_control.ae",
    "tests/run/batch_core_incremental.ae",
    "tests/run/batch_core_infra.ae",
    "tests/run/batch_core_io_primitives.ae",
    "tests/run/batch_core_link.ae",
    "tests/run/batch_core_memory.ae",
    "tests/run/batch_core_memory_advanced.ae",
    "tests/run/batch_core_memory_budget.ae",
    "tests/run/batch_core_memory_layout.ae",
    "tests/run/batch_core_metrics_advanced.ae",
    "tests/run/batch_core_modules.ae",
    "tests/run/batch_core_netpath.ae",
    "tests/run/batch_core_parsing.ae",
    "tests/run/batch_core_process_env.ae",
    "tests/run/batch_core_protocols.ae",
    "tests/run/batch_core_ranges.ae",
    "tests/run/batch_core_search_sort.ae",
    "tests/run/batch_core_semantic_analysis.ae",
    "tests/run/batch_core_semver.ae",
    "tests/run/batch_core_serialization.ae",
    "tests/run/batch_core_simd_math.ae",
    "tests/run/batch_core_structures.ae",
    "tests/run/batch_core_systems.ae",
    "tests/run/batch_core_unwind.ae",
    "tests/run/batch_core_wire_protocol.ae",
    "tests/run/bitwise_traits.ae",
    "tests/run/blocking_pool.ae",
    "tests/run/borrow_move_after_last_use.ae",
    "tests/run/borrow_mut_arg_after_last_use.ae",
    "tests/run/borrow_mut_call_after_cursor.ae",
    "tests/run/borrow_mut_call_after_last_use.ae",
    "tests/run/borrow_mut_call_read_before.ae",
    "tests/run/borrow_views.ae",
    "tests/run/borrow_write_after_last_use.ae",
    "tests/run/box_wrapper.ae",
    "tests/run/byte_streams.ae",
    "tests/run/cfg_loops.ae",
    "tests/run/chan_cancel.ae",
    "tests/run/chan_select.ae",
    "tests/run/channels.ae",
    "tests/run/checked_arithmetic.ae",
    "tests/run/clone_trait.ae",
    "tests/run/closures.ae",
    "tests/run/cmp_traits.ae",
    "tests/run/convert_traits.ae",
    "tests/run/csv_roundtrip.ae",
    "tests/run/deadline_race.ae",
    "tests/run/default_trait.ae",
    "tests/run/display_trait.ae",
    "tests/run/divide_by_zero.ae",
    "tests/run/duration_primitives.ae",
    "tests/run/enum_generic_impls.ae",
    "tests/run/enum_methods.ae",
    "tests/run/env_typed.ae",
    "tests/run/error_trait.ae",
    "tests/run/extern_alloc_ok.ae",
    "tests/run/extern_alloc_oom.ae",
    "tests/run/extern_c_call.ae",
    "tests/run/field_slices.ae",
    "tests/run/field_vector_loops.ae",
    "tests/run/fake_clock.ae",
    "tests/run/fake_io.ae",
    "tests/run/function_types.ae",
    "tests/run/generic_impl.ae",
    "tests/run/generic_layout_query.ae",
    "tests/run/generic_owned.ae",
    "tests/run/hash_trait.ae",
    "tests/run/iterator_trait.ae",
    "tests/run/json_encode.ae",
    "tests/run/json_validate.ae",
    "tests/run/log_basic.ae",
    "tests/run/methods.ae",
    "tests/run/mut_param_fields.ae",
    "tests/run/mutex_guard.ae",
    "tests/run/native_layout_abi.ae",
    "tests/run/nested_generics.ae",
    "tests/run/nested_map_receivers.ae",
    "tests/run/num_traits.ae",
    "tests/run/ops_traits.ae",
    "tests/run/option_prelude.ae",
    "tests/run/owned_string_fields.ae",
    "tests/run/owned_struct_return.ae",
    "tests/run/pair_product.ae",
    "tests/run/ptr_offset.ae",
    "tests/run/ptr_retag.ae",
    "tests/run/rand_minstd.ae",
    "tests/run/range_iter.ae",
    "tests/run/regex_match.ae",
    "tests/run/self_type.ae",
    "tests/run/shared_box.ae",
    "tests/run/slice_view.ae",
    "tests/run/struct_borrow.ae",
    "tests/run/struct_mut_borrow.ae",
    "tests/run/struct_value_params.ae",
    "tests/run/sync_markers.ae",
    "tests/run/tasks_advanced.ae",
    "tests/run/tasks_fifo.ae",
    "tests/run/test_tally.ae",
    "tests/run/thread_failure.ae",
    "tests/run/threads.ae",
    "tests/run/trait_bounds.ae",
    "tests/run/unsafe_block.ae",
    "tests/run/unsafe_fn_call.ae",
    "tests/run/unsafe_pointer_deref.ae",
    "tests/run/unsafe_pointer_write.ae",
    "tests/run/vector_out_of_bounds.ae",
]


def default_binary(repo):
    target = os.environ.get("CARGO_TARGET_DIR")
    if not target:
        try:
            result = subprocess.run(
                ["cargo", "metadata", "--no-deps", "--format-version", "1"],
                capture_output=True, text=True, cwd=repo)
            if result.returncode == 0:
                import json
                target = json.loads(result.stdout)["target_directory"]
        except OSError:
            target = None
    return os.path.join(target or os.path.join(repo, "target"), "debug", "ae")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--ae", default=None, help="path to the ae binary")
    parser.add_argument("--repo", default=os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    options = parser.parse_args()
    ae = options.ae or default_binary(options.repo)
    if os.name == "nt" and not ae.endswith(".exe"):
        ae += ".exe"
    failures = []
    for relative in FILES:
        source = os.path.join(options.repo, relative)
        result = subprocess.run([ae, "check", source], capture_output=True, text=True, errors="replace")
        if result.returncode != 0:
            failures.append((relative, result.stderr.strip().splitlines()[0] if result.stderr.strip() else "no output"))
    print(f"deny-legacy: {len(FILES) - len(failures)}/{len(FILES)} files are legacy-free")
    if failures:
        for relative, message in failures:
            print(f"legacy body remains in {relative}: {message}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())