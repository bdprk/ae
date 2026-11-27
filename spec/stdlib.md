# Standard Library and Built-in Migration Specification (M1.10 / M1.11 / M3.1 target)

Status: **target specification** — not yet implemented. Per the plan's Spec-First rule this document lands before the implementation; each section gains a positive/negative test pair as its slice merges. It transcribes decisions already made in the plan (§2.2 hard rules, §4.1 layering, §5.3 freeze list and migration policy) into enforceable rules, grounded in the exact inventory of today's built-ins.

## 1. Design Rules

### Hard Rules:
1. **Implement Once, Use for Any Type** (plan §2.2.1): containers, result types, and iterators are generic at the standard-library level. Hand-written container families per element type and pre-defined result enums are prohibited from this point on (the freeze is already in force, plan §5.3).
2. **Minimal Built-ins** (plan §2.2.2): the compiler retains only primitives that cannot be expressed in the language — memory, arithmetic, syscall bridges (I/O, process, clock). Everything else becomes standard-library modules invoked via method syntax.
3. **No Behavior Change in Migration**: a migrated built-in and its stdlib replacement are observably identical (outputs, exit codes, diagnostics) unless a documented improvement supersedes them, in which case the old name's alias keeps the old behavior for its one-version grace period.
4. **The Reserved-Name Set Shrinks Monotonically**: every name removed from `RESERVED` stays removed; a deprecated alias is an ordinary library name, not a compiler built-in.

### Non-Goals (this cycle):
- No package registry (M3.6) — this document covers the in-tree standard library only.
- No concurrency/networking primitives (`thread`, `task`, `net` — M4 scope, listed here only as reserved layer names).

## 2. Layering (plan §4.1)

| Layer | Depends on | Contents |
|---|---|---|
| `core` | nothing (no allocation, no OS) | primitive type methods, `Option[T]`/`Result[T, E]`, iterators, slices, `str`, formatting traits, atomics |
| `alloc` | `core` | `Box[T]`, `Vec[T]`, `String`, `Map[K, V]` (SwissTable style), `Set[T]`, `Deque[T]`, `BTreeMap[K, V]`, `Rc[T]`, `Arc[T]`, `Arena` |
| `std` | `core` + `alloc` | `fs`, `path`, `io` (buffered, streams), `env`, `process`, `time`, `thread`*, `task`*, `sync`*, `net`*, `fmt`, `hash`, `rand`, `json`, `csv`, `regex`, `log`, `test` |

\* M4 deliverables; the layer slots are reserved so M3 does not paint itself into a corner.

Official extensions versioned independently (plan §4.1): `http`, `tls`, `sqlite`, `postgres`, `serde`, `cli`, `tensor`, `gpu`, `gui`, `python`.

## 3. The Migration Inventory

The compiler's reserved-name registry (`RESERVED`, `crates/ae-compiler/src/frontend/mod.rs`) is the exact surface to dissolve. Every name below migrates to a method or module function; the compiler keeps only the primitives marked **[keep]**.

| Domain | Today's built-ins | Destination |
|---|---|---|
| Containers | `vec` literals, `vec[string]`/`vec[f64]`, `vec[T]` for Copy/owned/nested elements (ADR-0235), `strvec_new`, `f64vec_new`, `push`, `pop`, `sort`, `reverse` | `alloc::Vec[T]` — `v.push(x)`, `v.pop()`, `v.sort()`, `v.reverse()`; `vec[T]::new()` |
| Maps | `map_new`, `f64map_new`, `map_put`, `map_get`, `map_has`, `map_remove`, `map_inc`, `keys` | `alloc::Map[K, V]` — `m.put(k, v)`, `m.get(k) -> Option[V]`, `m.has(k)`, `m.remove(k)`, `m.entry(k).or_insert(0)`, `m.keys()` |
| Strings | `bytes`, `contains`, `slice`, `split`, `join`, `lines`, `count_lines_with`, `trim`, `starts_with`, `ends_with`, `find`, `utf8_len`, `utf8_prefix`, `to_string`, `parse_int`, `slice_view`, `tokenize` | `core::str` / `alloc::String` — `s.contains(p)`, `s.split(sep)`, `s.find(p) -> Option[usize]`, `s.trim()`, `s.lines()`, `s.bytes()`, `s.parse[T]()`, `s.tokenize(sep)` |
| Numbers | `to_f64`, `to_i64`, `sqrt`, `abs`, `floor`, `ceil`, `round`, `exp`, `log`, `sin`, `cos`, `pow`, `atan2`, `min`, `max` | `core` numeric methods — `n.to_f64()`, `f.sqrt()`, `f.min(o)`; `min`/`max` keep the NaN-ignoring, ±0-total-order semantics (ADR-64) |
| Files & paths | `read_file`, `write_file`, `try_read_file`, `try_write_file`, `try_append_file`, `try_list_dir`, `try_file_info`, `try_copy_file`, `try_rename_path`, `try_remove_file`, `try_create_dir`, `try_remove_dir`, `path_exists`, `path_is_dir`, `path_join`, `path_name`, `path_parent` | `std::fs` / `std::path` — `fs::read_to_string(path)?`, `fs::write(path, text)?`, `path.join(name)` |
| Environment & process | `env`, `args`, `cwd`, `set_cwd`, `read_stdin`, `run_command`, **[keep]** `exit` | `std::env` / `std::process` — `env::get(name)?`, `env::args()`, `process::run(cmd)` |
| Output | **[keep]** `print` (as `print`/`println`, the console bridge), `assert` (moves to the `test` convention) | `std::fmt` for the interpolation form `"{x:>8.2}"` (M1.12, compile-time parsed) |
| JSON | `json_get`, `json_int`, `json_bool`, `json_len`, `json_has` | `std::json` — the pure-AE writer (`std/std/json.ae`: `encode_string`/`encode_bool`/`encode_i64`/`encode_f64`/`encode_null`/`encode_array`/`encode_object`/`encode_field`, plus the numeric-array helpers) emits documents today and a pure-AE structural validator (`json_valid`) rejects a malformed document before `json_get` (P4.4); `json::get(doc, "$.a.b")` reading survives until derive-based serialization (M3.3) replaces it |
| CSV | (no builtin today) | `std::csv` — pure-AE `encode_field`/`encode_row`/`parse_row` (P4.4) write and read an RFC-4180-style row with quote/comma/newline escaping; a streaming multi-line reader remains |
| Random | (no builtin today) | `std::rand` — pure-AE Park-Miller MINSTD (`seed_state`/`next_state`/`next_below`/`next_byte`, threaded `Rng`); deterministic from a seed, overflow-free (P4.4) |
| Regex | (no builtin today) | `std::regex` — pure-AE backtracking matcher over UTF-8 code points: `regex_match` (search anywhere) and `regex_full_match`, with literals, `.`, classes/ranges/negation, `\d \w \s`, escaped literals, greedy `*`/`+`/`?`, counted `{m}`/`{m,}`/`{m,n}`, `^`/`$` and top-level `|` alternation (P4.4; capture groups and lazy quantifiers remain) |
| Logging | (no builtin today; `LogLevel`/`log_level_code`/`log_is_enabled` in `std/core/log.ae`) | `std::log` — pure-AE `log_level_name`/`log_record`/`log_write` and `log_debug`/`log_info`/`log_warn`/`log_error` emitting `LEVEL tag: message` (P4.4; the sink is the stdout console bridge until an `eprintln`/stderr intrinsic exists) |
| FFI layout | **[keep]** `c_size_of`, `c_align_of`, `c_offset_of` | stays with the `#[repr(C)]` family (`spec/unsafe.md` §5) — not a stdlib item |

The nine preset result enums (`ReadResult` … `FloatResult`) and the shape-compatible custom-enum contract dissolve into `Result[T, E]` + `Option[T]` (M1.11, P2.4): every preset family is the `Result[payload, string]` instantiation of the one generic declaration, the family names remain pattern aliases, and the C runtime's macro ids are generated from the registered enum ids (ADR-0233). Fallible built-ins therefore return `Result`-shaped values, `?` propagates the one generic type, and the one-family-per-function restriction disappears with it.

## 4. Migration Policy (plan §5.3)

1. Each slice lands one domain: spec + positive/negative tests → implementation → examples migrated → snapshots blessed → ADR.
2. Old names remain **deprecated aliases** for one version: they compile, emit a deprecation warning naming the replacement, and are removed at the next version boundary.
3. The alias layer is written in the language itself (wrapper functions/methods), not in the compiler — so removal deletes library code, not compiler surface.
4. `examples/` migrates per slice; the Go-comparison set (M1.13) runs only after the last alias is gone.
5. Performance gate (M1.14): map/text/sum workloads after migration must be within 10% of the pre-freeze baseline (benchmark report per change batch).

## 5. Acceptance Criteria

| From | Criterion |
|---|---|
| M1.10 | `Vec[T]`, `String`, `Map[K, V]`, `Set[T]`, `Option`, `Result` implemented in the language itself; `vec[f64]`, `strvec_new`, `f64map_new` etc. removed with aliases in place; runtime retains only type-agnostic primitives |
| M1.11 | Unified `Result[T, E]`, `std::Error` trait, `.context()`; all nine result enum families gone from the compiler; every `examples/` program migrated |
| M3.1 | `core`/`alloc`/`std` layered per §2 with doc comments and doc tests; `ae doc` generates from them |
| M3.3 | `json` (derived), `csv`, `regex`, `log`, `rand`, `time` benchmarked against the Go standard library |

## 6. Test Matrix (per slice)

| # | Case | Expected |
|---|---|---|
| S1 | A migrated built-in and its deprecated alias produce identical output on the shared corpus | Byte-identical stdout, same exit codes |
| S2 | A deprecated alias compiles | Warning naming the replacement, not an error |
| S3 | A removed (post-grace) alias | Rejected with the `mark it 'pub'`-class clarity — an unknown-name diagnostic suggesting the stdlib path |
| S4 | Generic container over a user struct: `Vec[Point]` push/pop/iterate/sort | Runs; no per-type compiler support was added |
| S5 | `Map[K, V]` with a generic key type through `Hash`/`Eq` bounds | Runs |
| S6 | `m.entry(k).or_insert(0) += 1` histogram (plan §2.3 shape) | Runs; single lookup |
| S7 | `Result[T, E]` propagated through `?` across module boundaries | Compiles; no family restriction |
| S8 | `core` layer compiles with no allocator linked (empty program artifact within budget) | Artifact size report |
| S9 | Doc comments on every public stdlib item; `ae doc` renders them | Generated doc tree |
| S10 | Doc tests run under `ae test` | Pass on both platforms |
