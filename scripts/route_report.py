#!/usr/bin/env python3
"""Body-route coverage report (checklist P1.1).

The front end reports every function or closure body that still falls back
to the typed route when `AE_ROUTE_REPORT` is set:

    route legacy function id=<n> at <offset>

This script runs `ae check` over the corpus (examples, tests/run, std/std),
counts those lines, and prints a stable one-line summary:

    fallbacks=<n> files=<m> checked=<k> failed=<f>

`files` counts the sources with at least one fallback; `checked` the sources
that still pass `check`. A file that errors early reports nothing — the std
module files that only check inside their importer are the expected set — so
a failed check is listed for information but does not change the count.
`--details` prints every line. `--max` turns the number into a ratchet: the
run fails when the fallback total rises above the given ceiling, which keeps
a route regression from slipping in unnoticed between slices.
"""

import argparse
import os
import subprocess
import sys

ROOTS = ["examples", "tests/run", "std/std"]
MARKER = "route legacy "
TYPED_MARKER = "route typed-expression "


def default_binary(repo):
    """The `ae` binary: CARGO_TARGET_DIR wins (this workspace keeps its build
    cache outside the tree), then cargo's own answer, then `target`."""
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


def sources(repo):
    for root in ROOTS:
        base = os.path.join(repo, root)
        for directory, _, names in os.walk(base):
            for name in sorted(names):
                if name.endswith(".ae"):
                    yield os.path.join(directory, name)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--ae", default=None, help="path to the ae binary")
    parser.add_argument("--repo", default=os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    parser.add_argument("--details", action="store_true")
    parser.add_argument("--max", type=int, default=None, help="fail when the fallback total exceeds this ceiling")
    parser.add_argument("--max-typed", type=int, default=None, help="fail when the typed-expression fallback total exceeds this ceiling")
    options = parser.parse_args()
    ae = options.ae or default_binary(options.repo)
    if os.name == "nt" and not ae.endswith(".exe"):
        ae += ".exe"
    env = dict(os.environ, AE_ROUTE_REPORT="1")
    fallbacks = 0
    files = 0
    typed = 0
    typed_files = 0
    checked = 0
    failed = []
    for source in sorted(sources(options.repo)):
        relative = os.path.relpath(source, options.repo)
        result = subprocess.run([ae, "check", source], capture_output=True, text=True, env=env)
        lines = [line for line in result.stderr.splitlines() if MARKER in line]
        typed_lines = [line for line in result.stderr.splitlines() if TYPED_MARKER in line]
        if result.returncode != 0:
            failed.append(relative)
            continue
        checked += 1
        fallbacks += len(lines)
        typed += len(typed_lines)
        if typed_lines:
            typed_files += 1
            if options.details:
                for line in typed_lines:
                    print(f"{relative}: {line}")
        if lines:
            files += 1
            if options.details:
                for line in lines:
                    print(f"{relative}: {line}")
    print(f"fallbacks={fallbacks} files={files} typed={typed} typed_files={typed_files} checked={checked} failed={len(failed)}")
    for relative in failed:
        print(f"check skipped (fails standalone): {relative}")
    if options.max is not None and fallbacks > options.max:
        print(f"route coverage regressed: {fallbacks} > {options.max}", file=sys.stderr)
        return 1
    if options.max_typed is not None and typed > options.max_typed:
        print(f"typed-expression fallbacks regressed: {typed} > {options.max_typed}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())