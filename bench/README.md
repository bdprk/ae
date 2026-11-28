# Exploratory Benchmarking

`python bench/run.py` tests development mode, `python bench/run.py --release` tests optimized mode, and adding `--cold` forces each ae build to use a fresh, empty cache directory (measuring a clean compilation). `--workload sum|text|map|all` selects the workload (defaulting to all three). The script utilizes native Python, Rust, GCC, and Go, first compiling the ae compiler, then compiling/running each implementation while checking every timed run output and a separate final run. It records wall-clock time, executable size, direct child process CPU time (on platforms supporting `getrusage`), and peak memory for every build and run. Repetitions are interleaved across languages within each round. The process uses the system temporary directory, and results are output as JSON.

Three workloads:

- `sum`: One million additions with overflow checks, covering ae/Go/Rust; serves as the minimal pure-computation control.
- `text`: Reads a generated 2.6 MB, 50,000-line ASCII text file, splits by whitespace, and outputs the byte count, token count, and the sum of bytes within tokens. Summing token lengths ensures every byte is read, preventing optimization via "print file size only." Python is included here because text processing is the scenario compared in design proposal §14.2—the trade-off is that its runtime includes interpreter startup, and it lacks a build phase (`build_*` fields are null).
- `map`: Uses a fixed-seed LCG to generate one million extractions, writes to a `map[string, i64]` histogram with 10,000 string keys, and then reads all buckets using a checksum weighted by counts; `len` reports how many keys were reached. The checksum cannot be derived from the extraction count or key count, so every extraction and lookup is mandatory. Python is included because dict counting is the control scenario for container and allocation overhead; the ae implementation's `Err` branch prints nothing, and missing keys are treated as 0 contributions, consistent with the Go/Python/Rust twin programs.

Peak memory is captured from the OS high-water mark (Windows `GetProcessMemoryInfo`'s `PeakWorkingSetSize`, Linux `/proc/<pid>/status`'s `VmHWM`), sampled every millisecond: as long as one sample succeeds before exit, the true peak is captured; processes shorter than the sampling interval may report null. Only direct child processes are sampled during builds, so `cc1`/`as`/`ld` spawned by `gcc` and `rustc`'s codegen subprocesses are **not included**; the semantics of this metric are "peak of the compiler driver itself," not the peak of the entire build tree.

This is merely to validate the measurement pipeline, not a language performance ranking: inputs are small; in development mode, ae uses GCC `-O0` and Rust uses `opt-level=0`; optimized mode uses `-O2` and `opt-level=2` respectively, while Go always uses its default build; identical numeric optimization levels do not imply equivalent optimization. ae's `build` reuses a content-addressed cache: without `--cold`, repeated builds measure the cache-hit path; use `--cold` for cold builds. External toolchain cache states are uncontrolled; repetitions are interleaved across languages but not randomized, and thermal/power policies are not controlled; wall-clock time includes process startup overhead. Go manually checks for addition overflow, Rust uses `checked_add`, and ae enables built-in checks; error handling approaches still differ. CPU time is only recorded on platforms supporting `resource` and covers only direct child processes (e.g., the `gcc` driver itself; grandchild processes like `cc1`/`as`/`ld` are excluded). Cache size and thermal/power policies remain unmeasured. Linux figures are outside the scope of this script.

The latest round of results is indexed in `bench/results/<date>-<machine>/PLATFORM.txt` (including environment, raw JSON, and observations). Current quantifiable facts: ae build time under a hot cache is determined by cache hits and cannot be used as evidence of compilation speed; in the `text` workload, ae's peak runtime memory is significantly higher than Rust's because `split` allocates a string for every token; the primary cause of `map` workload runtime is string construction rather than map operations (see `2026-11-11-windows-map-hotspots/` for hotspot breakdown). Three optimizations targeting this have been verified on both platforms: literal concatenation fusion (ADR-44), `to_string` stack buffering (ADR-45), and `map_inc` counting built-in (ADR-46), with cumulative map variant improvements of -23.4% and counting shape improvements of ~-32% (`2026-11-11-{windows,linux}-litcat/`, `-intbuf/`, `-map-inc/`).

Future requirements: longer and more diverse real-world workloads (the `map` count is added, but file organization workflows are still missing), refinement of repetition/randomization strategies, and recording of environmental factors like temperature; build peak memory needs to cover the entire process tree (job object / cgroup); and maintain reproducible definitions for cold/hot caches. Record the full environment and raw results. Run output can be redirected outside the project or to a user-selected file to avoid accidentally committing machine environment data to the repository.

`python bench/phases.py` compiles the generated C into object files separately, then links them separately, running each five times and verifying the output. This experiment uses two GCC invocations, which is not strictly equivalent to a normal full build in a single invocation; results include process startup time and uncontrolled cache states.
M0 report format: see `bench/reports/TEMPLATE.md`. A first single-platform sum
baseline and its raw samples are in `bench/reports/2026-11-12-windows-sum-baseline.md`.
Historical measurements remain under `bench/results/`.

The runner validates each timed run against the fixed `sum` result or the
ae result for the same round. The separate memory pass is not output
validated. Build memory passes use separate executable paths, and cold ae
measurements use separate empty caches for timing and memory. Memory-pass failures
abort the run rather than being reported as unavailable samples. Reports
created before this change retain their original measurement limitations.

Since ADR-0177 the driver also caches the generated C per source-file set, so a
warm ae build no longer runs the frontend at all. Warm and no-modification
build numbers from before that change therefore measured a different pipeline
and are not like-for-like with later runs; `--cold` is unaffected. The cost
removed by that cache on one synthetic 3,002-line program is in
`bench/reports/2026-11-12-windows-frontend-cache.md`.
