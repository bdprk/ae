# Package Management Specification (M3.4 / M3.6 target)

Status: The CLI builds dependency-free packages and inspects local path dependency graphs. Check, run and locked builds support local path dependencies. Registry version resolution and publishing specify future milestones.

## 1. Design Rules

### Hard Rules:
1. **Determinism Above Convenience**: the same `ae.lock` always produces the same build. Resolution is **Minimal Version Selection (MVS)** — no SAT solver, no version "solve": every dependency gets the minimum version that satisfies all declared requirements, and the lockfile records the result.
2. **One Tool, One Command Away**: everything lives in the single `ae` executable (`new`, `add`, `remove`, `update`, `publish`); a new user completes a CLI app with 2 dependencies in 5 minutes (M3.6 acceptance).
3. **Content Is What Counts**: every registry artifact is addressed and verified by content hash; a mirror or registry serving different bytes for the same hash is a hard error.
4. **The Build Knows Its Inputs**: dependency sources (registry, git, path) feed the existing interface-summary/cache machinery (M3.5) — a package is a compilation unit with a manifest, not a parallel build system.

### Non-Goals (this cycle):
- No custom build scripts inside dependencies (a package is ae source + manifest; codegen hooks are future work and need their own ADR).
- No semver ranges beyond the MVS subset (`^`-style compatible ranges); no `*` wildcards in published manifests.
- No mutable registries: a published version is immutable; yanking removes it from resolution, never from download.

## 2. Manifest (`ae.toml`)

```
[package]
name = "log-analyzer"
version = "0.3.1"
ae = "1.0"          # toolchain compatibility floor
edition = "2"       # source edition; absent = 1

[dependencies]
json = "1.2"            # registry dependency, MVS-compatible range
csv  = "0.9"
regex = { git = "https://github.com/org/regex", rev = "3f2a..." }
tools = { path = "../tools" }
net  = { version = "2.0", platform = "windows" }   # platform-conditional

[target.'cfg(unix)'.dependencies]
posix = "1.0"
```

- `[package]` requires `name` (lowercase, `[a-z0-9_-]`, ≤64 bytes), `version` (three-number semver, no prerelease in this cycle), and the `ae` floor.
- `[package] edition` is optional. `"2"` is the current edition and `"1"` is what a manifest without the field means: edition 1 keeps the pre-edition behavior (deprecated aliases compile silently, `spec/stdlib.md` §4), edition 2 warns. The edition follows the declaring package, so a new-edition package may consume an old-edition one; a file compiled without a manifest is edition 2, and `ae new`/`ae init` scaffold `edition = "2"`.
- A path dependency table accepts `path` and an optional `version` caret range (`0.3` admits `0.3.0` through `0.3.9` but not `0.4.0`; `0.0.3` admits only `0.0.3`). Resolution rejects a dependency whose manifest version falls outside the range, naming the alias, the requirement and the version found; unknown dependency keys are rejected rather than ignored.
- Registry dependencies take an MVS range; git deps pin a `rev` (branch/tag names are rejected — the lockfile must be reproducible without consulting the remote's mutable state); path deps are for in-repo development and are **rejected in `publish`**.
- Platform-conditional dependencies select per target; the condition subset is `{windows, unix, x86_64, arm64}` until M7 needs more.

Pass a package directory or its ae.toml path to check, run, build, emit-c, emit-tree, emit-cfg, or emit-mir. Omit the source argument to use the nearest ae.toml in the working directory or its parents. The entry is src/main.ae relative to the manifest. check --json and run --interp use the same entry. The ae field accepts a major.minor floor with optional patch. Local path dependencies can appear in a target table. The native toolchain selects tables for its own OS and architecture. Conflicting declarations for the same active alias are rejected. Build with local dependencies requires a current ae.lock.

ae new DIRECTORY creates a package with version 0.1.0 and a compatibility floor matching the installed compiler. It takes the package name from the final directory component and creates src/main.ae. The directory must be new, and its parent must exist. ae init [DIRECTORY] scaffolds in a directory that already exists: it writes the manifest, creates src/, and writes src/main.ae only when that file is absent, so adopting a directory of sources never overwrites them; a directory that already has ae.toml is refused. The package name comes from the directory in both cases.

ae deps accepts a package path or discovers the nearest manifest. It follows path dependencies relative to each declaring manifest, lists dependencies before their dependents and prints each canonical package once. Cycles report the chain of manifests. Registry and git dependencies remain future work.

Use `use "@alias";` to load a declared path dependency's src/lib.ae. Each import resolves against the declaring package's direct dependency table. The existing compiler module rules apply to symbols loaded from those files. Check, run and emit commands support library imports. Frontend cache reuse checks both the resolved import map and all previously loaded source hashes. Locked builds verify their lockfile even when the frontend cache hits.

For an application package, ae test reads src/test.ae and uses the same dependency aliases as src/main.ae. Test entry files use the existing test harness convention and omit main. The --interp option runs the same tests in the interpreter.

`tests/packages/` is the vendored downstream corpus (M8.1.05): one shared edition-1 library and two applications, one per edition, each with a committed lockfile and a saved baseline. The corpus test rebuilds them through `ae build` (verifying every lock hash), compares the compiled and interpreted output against the baseline, and fails closed when a source is edited without `ae update`.

## 3. Lockfile (`ae.lock`)

The locked graph is independent of the order dependencies are declared in and of the working directory `ae update` runs from; the graph, not the text, is what resolution produces. The root manifest is still content-addressed, so any edit to `ae.toml` — including moving one dependency line — makes an existing lock stale and requires `ae update`.

For local packages, ae update loads the application and writes ae.lock atomically. The current format records SHA-256 for each manifest in dependency order and for each source file loaded by the compiler. Paths are relative to the package root. Build checks these records against the loaded inputs before invoking the C compiler. Check and run permit working changes without updating the lock. A dependency-free project can build without a lock.

- Generated by resolution, never hand-edited: packages, resolved versions, content hashes, sources (registry URL/rev/path), and the dependency graph in topological order.
- `ae build` verifies every hash before compilation; a mismatch fails with the offending package and the two hashes.
- The lockfile is committed alongside `ae.toml` for applications; libraries commit it too (MVS uses the union of requirements, not their locks).

## 4. Resolution (MVS)

- Input: the root manifest plus, for each candidate version, its own requirements — transitively, cycle-checked (dependency cycles are rejected at resolution with the cycle path).
- One name, one version: two manifests declaring the same package name at different versions are rejected, naming both manifests — a program cannot link two versions of one package, and the second copy would otherwise silently shadow the first. The same name at the same version reached through different paths is tolerated; each copy compiles from its own sources.
- Selection: for each package, take the **minimum version satisfying every requirement** that mentions it. The lockfile pins exactly these.
- `ae update` re-resolves and rewrites the lock (bumping within ranges); `ae add pkg@ver` adds the requirement and resolves in the same pass.

## 5. Registry

- **Static index + object storage**: the index maps `name` → sorted list of `(version, requirements, content hash, storage key)`; artifacts are immutable blobs.
- Publishing (`ae publish`) uploads a source archive (the package's ae files + manifest, no `target/`, no lock) and appends to the index; path deps, missing fields, or a re-upload of an existing version fail.
- Signing and revocation policies land with the registry prototype (M3.6) and follow the plan's security posture; until signing exists, the content hash is the only trust anchor and the registry URL is pinned in the lockfile.

## 6. Diagnostics

| Situation | Behavior |
|---|---|
| Unsatisfiable requirements | Rejected at resolution: the two requirements, the versions each admits, and the dependents that carry them |
| Dependency cycle | Rejected with the cycle path |
| Hash mismatch | Rejected before compilation, naming the package and both hashes |
| Missing lock entry for a manifest requirement | Rejected — run `ae update`; the build never silently resolves |
| Path dependency in `publish` | Rejected (path deps are development-only) |
| Registry unreachable | `AE_OFFLINE=1` builds from the lock against a local mirror/cache; online builds re-verify hashes |

## 7. Test Matrix

| # | Case | Expected |
|---|---|---|
| K1 | `ae new` + two registry deps build offline after one online resolve | Builds; second resolve touches no network |
| K2 | MVS: two packages require `json@^1.1` and `json@^1.2` | Resolves to the minimum satisfying both; lockfile records it |
| K3 | Two requirements with disjoint ranges | Rejected, naming both dependents |
| K4 | Dependency cycle a→b→a | Rejected with the cycle path |
| K5 | Corrupted artifact (hash mismatch) | Rejected before compilation |
| K6 | Manifest edited, lock not updated | Rejected with "run ae update" |
| K7 | Git dep with a branch instead of `rev` | Rejected at manifest parse |
| K8 | `publish` with a path dependency | Rejected |
| K9 | Re-publish of an existing version | Rejected (registry immutability) |
| K10 | Platform-conditional dep on the other platform | Excluded from resolution and build |
| K11 | `ae update` bumps within ranges only | Lock changes, no requirement violated |
| K12 | Cache: modifying a dependency's body recompiles only that package (with M3.5) | Interface hashes govern |

## 8. Acceptance

| From | Criterion |
|---|---|
| M3.4 | Manifest/lock/MVS/hash verification + dependency resolution test suite green |
| M3.5 | Package-level compilation participates in the interface-summary cache: modifying one dependency's function body recompiles exactly that package |
| M3.6 | `ae new/add/update/publish` + registry prototype; a new user completes a CLI app with 2 dependencies in 5 minutes |
