# Pure C backend vs legacy MIR backend — benchmark comparison (2026-11-27)

Host: Windows x86-64, MinGW gcc 16.2 (`-O2`), 16 cores, measurements taken with
the machine otherwise idle (the differential-fuzz campaigns were stopped for the
runs). Each cell is the median of three to four runs of the benchmark binary;
the sources are the seven ported programs in `bench/gojo-suite/ae/` (the
`binarytrees` port is still missing — see "Coverage gap" below).

Command shapes:

```sh
ae build bench/gojo-suite/ae/<name>.ae -o <legacy>            # legacy MIR backend
ae build --pure bench/gojo-suite/ae/<name>.ae -o <pure>       # pure-MIR C backend
```

## Results (ms)

| Benchmark | Legacy MIR | Pure MIR | Pure speedup |
| --- | ---: | ---: | ---: |
| fib(40) | 530 | **305** | 1.74× |
| fannkuch(10) | 620 | **215** | 2.88× |
| spectralnorm(2000) | 695 | **490** | 1.42× |
| sort(5M i64) | 445 | 450 | tie |
| mandelbrot(2000) | 333 | 341 | tie |
| nbody(5M) | 313 | 376 | 0.83× (pure slower) |
| hashmap_str | 200 | 195 | tie |

The sort row is measured after the 2026-11-27 runtime change that replaced
`qsort` + function-pointer comparators with a specialized inline quicksort for
i64 vectors: the same benchmark fell from **775 ms → 445 ms (1.74×)** on the
legacy backend and now sits at ~450 ms on the pure backend.

## Reading

- The pure backend's direct expression emission (no MIR block temporaries, no
  per-statement local slots) and its `static inline` runtime helpers pay off on
  recursion- and index-heavy loops: fib, fannkuch and spectralnorm are the
  three biggest wins.
- `nbody` is the one regression (pure ~20% slower). The benchmark is dominated
  by float struct field access, which is the first place to profile next; the
  measurement above does not isolate the cause.
- mandelbrot and hashmap are within noise; both are dominated by arithmetic or
  map operations the two backends spell the same way.

## Relation to the external five-language report (2026-11-27)

The external report (`gojo-zig-moonbit-benchmark/outputs/REPORT.md`) measured ae
with the **legacy** backend and showed ae at 2.05× geometric mean, with fib 3.5×
and spectralnorm 3.0× behind Gojo. Two factors explain that gap:

1. **Backend choice.** The same machine measures the pure backend 1.7× (fib) to
   2.9× (fannkuch) faster than legacy. `ae build --pure` / `ae run --pure`
   (commit `9d978ddb`) expose it; a re-run should use it or the default should
   switch after the ADR-0242 exit.
2. **Checked arithmetic is a language contract.** ae traps on i64 overflow
   (append-only, documented); Gojo, Go and Zig ReleaseFast wrap silently. The
   fib cell is therefore not an apples-to-apples codegen comparison. The report
   should mark that column as "checked arithmetic" (or add a wrapping-column
   variant) before drawing conclusions about codegen quality.

## Coverage gap: binarytrees

A faithful port needs a recursive owned type (a node owning its children).
The language has raw pointers and `unsafe`, but a struct cannot contain itself,
and `Box[T]`/`Rc[T]` are still stubs (P4.1). Encoding the tree as an index arena
over `vec` would change the benchmark's meaning (allocation density and pointer
chasing), so the ae column stays `—` until P4.1 lands; that is a language
capability gap, not a missing port.