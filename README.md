# ae

ae is an independently developed, experimental native programming language, currently
targeting Windows and Linux x86-64.

It aims to combine Go's compile speed with Rust's runtime performance and safety, while
keeping CPU, memory and disk footprint small. Safety and correctness are non-negotiable;
runtime cost, compile speed and binary size are optimized jointly against benchmark
budgets. A broad ecosystem is a long-term but secondary goal.

Status: a working prototype with concurrency (OS threads, channels, atomic
cells, a cooperative executor and a worker pool), not yet a complete
memory-safe language (no formal safety proof). Exact rules and limits live in
[Current Language Semantics](docs/current-language.md).

## What works

- **Core language**: functions, `let`/`var`, integers/`f64`/`bool`/`string`,
  checked arithmetic (overflow and division-by-zero trap with source location),
  `if`/`else`, `while`, `for i in a..b`, `break`/`continue`, `assert`.
- **Data**: fixed arrays and dynamic `vec`, both fully bounds-checked; structs;
  enums with exhaustive `match`; slices `&[T]` and `&mut [T]`.
- **Memory**: move semantics with deterministic deallocation; borrow checking for
  `&T`, `&vec`, `&string` and zero-copy string views.
- **Abstraction**: modules (`use`/`pub`), monomorphized generics with constraints
  (`Add`/`Eq`/`Ord`/`Copy`), traits and impls with static dispatch, `Option[T]` and
  `?` propagation, plus a family of recoverable `*Result` types.
- **Strings**: `split`/`join`/`lines`/`slice`/`find`/`trim`/`contains`/
  `starts_with`/`ends_with`/`count_lines_with`, plus zero-copy `slice_view` and the
  zero-allocation streaming cursor `tokenize`.
- **Containers**: `vec[T]`, `map[string, i64]`, `vec[f64]`, `map[string, f64]`.
- **Concurrency**: `spawn`/`wait`, channels (bounded, rendezvous, `select`),
  `atomic` cells, shared boxes, mutexes with guards, `exec`/`task_spawn`/
  `task_yield`, scoped tasks and a worker pool.
- **Files, process, system**: `read_file`/`write_file` and recoverable `try_*`
  variants, path algebra, directory traversal, file metadata, `run_command`,
  `args`, stdin, `cwd`, environment variables, wall-clock time.
- **Standard library**: `std/core`, `std/alloc`, `std/compiler` and hosted
  `std/std` modules, including the `fs`/`path`/`time`/`process`/`io` surfaces and
  a UTF-8 validator (`is_utf8`, `utf8_error_offset`).
- **Other**: JSON path queries, math built-ins (`sqrt`, `abs`, `floor`, `ceil`,
  `round`, `exp`, `log`, `sin`, `cos`, `pow`, `atan2`, `min`, `max`).

## Backends

Two C11 backends emit the same language semantics:

- the **legacy MIR backend** (the default for `build`/`run`);
- the **pure-MIR backend** (`emit-pure-c`, or `build --pure` / `run --pure`):
  a separate lowering with an independent C runtime. It is at output parity on
  the whole accepted corpus and the examples, and it is faster on
  recursion/index-heavy code (fib 1.7×, fannkuch 2.9×, spectralnorm 1.4× on the
  recorded host; see [the backend comparison](bench/reports/2026-11-27-pure-vs-legacy-windows.md)).

## Usage

Exit codes are part of the interface: `0` success, `2` bad command line, `1` the
program or package was rejected (diagnostics, I/O), and a compiled program's own
`exit(n)` is passed through. Requires Rust and a C11 GCC (`AE_CC` to override).
Verified natively on Windows (MinGW) and Linux.

```sh
cargo test
cargo run -- check examples/hello.ae
cargo run -- check --json examples/hello.ae   # stdout is exactly a JSON array
cargo run -- run examples/hello.ae
cargo run -- run examples/hello.ae --pure                    # pure-MIR C backend
cargo run -- build examples/arithmetic.ae -o arithmetic.exe   # -O2 (add --debug for -O0)
cargo run -- build examples/arithmetic.ae -o arithmetic.exe --pure
cargo run -- run examples/hello.ae --release                  # -O2
cargo run -- test tests_demo/strings_test.ae
cargo run -- emit-c examples/hello.ae
cargo run -- emit-pure-c examples/hello.ae
cargo run -- fmt examples/hello.ae
```

`build` and `run` share a content-addressed cache (`AE_CACHE` overrides the
location; `cache dir|clear|stats` manages it). The generated C keeps only the
runtime functions the program reaches, so emitted size varies by program.
`--pure` compiles through the pure-MIR backend and bypasses that cache (the
cached C belongs to the legacy backend); `--legacy` forces the default backend,
and `AE_BACKEND=pure` / `AE_BACKEND=legacy` does the same without a flag.

## Limitations

- No formal safety proof yet; the borrow checker is calibrated against the
  accepted corpus but a machine-checked model is future work.
- No async networking/reactor yet (`std/std/net` is not implemented).
- Traits are v1: static dispatch only — no method-call syntax, no `&T` trait
  parameters, no dynamic dispatch.
- `f64` has no implicit conversions; `vec`/`map` are otherwise integer-oriented.
- Module interface hashes do not yet participate in cache invalidation.
- Performance is benchmarked, not claimed: `fib` pays for checked arithmetic
  that the comparison languages do not perform. Numbers and caveats live in
  [the backend comparison](bench/reports/2026-11-27-pure-vs-legacy-windows.md).

## Docs

- [Current Language Semantics](docs/current-language.md) · [Resource Model](docs/resource-model.md)
- [Design Roadmap](docs/design.md) · [Decision Records](docs/decisions.md)
- [Platform Support Matrix](docs/platform-matrix.md) · [Benchmark Notes](bench/README.md)

## License

MIT, see [LICENSE](LICENSE).
