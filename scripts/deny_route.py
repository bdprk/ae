#!/usr/bin/env python3
"""The P1.1 endgame switch: files that must compile with the legacy parser
disabled (`AE_DENY_LEGACY_ROUTE=1`).

Every file listed here has no legacy body left — the resolved route covers
its functions and closures completely. Add a file once
`python scripts/route_report.py --details` shows no `route legacy` line for
it; from then on this gate keeps it legacy-free. When the list covers the
whole corpus, the legacy parser can be deleted (P1.2-P1.4).
"""

import argparse
import os
import subprocess
import sys

# Sorted so the manifest reads like a checklist; add new entries in place.
FILES = [
    "examples/borrow.ae",
    "examples/concat.ae",
    "examples/field_view.ae",
    "examples/heart.ae",
    "examples/map_count.ae",
    "examples/map_missing.ae",
    "examples/option_propagate.ae",
    "examples/sensor_stats.ae",
    "examples/strings.ae",
    "examples/struct_mut_borrow.ae",
    "examples/tokens.ae",
    "tests/run/mut_param_fields.ae",
    "tests/run/owned_string_fields.ae",
    "tests/run/struct_mut_borrow.ae",
    "tests/run/struct_value_params.ae",
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
    env = dict(os.environ, AE_DENY_LEGACY_ROUTE="1")
    failures = []
    for relative in FILES:
        source = os.path.join(options.repo, relative)
        result = subprocess.run([ae, "check", source], capture_output=True, text=True, errors="replace", env=env)
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