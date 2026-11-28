#!/usr/bin/env python3
"""Pure-route verification report (ADR-0243 stage 3 rehearsal).

With `AE_PURE_VERIFY=1`, `ae check` runs the pure MIR pipeline (lower ->
optimize -> validate -> borrow-check) after the frontend accepts a program and
reports a disagreement as an internal error. This script drives that over the
accepted corpus and fails when any program the language accepts is rejected by
the pure checker:

    programs=<n> accepted=<k> disagreements=<d>

`--details` prints every disagreement. The gate is the evidence the route
switch needs: a program the frontend accepts must lower and check, or the
switch would reject valid code.
"""

import argparse
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MARKER = "internal: the pure MIR checker disagrees"


def find_executable() -> Path:
    """The ae binary, from AE_PURE_VERIFY's sibling rules (cargo target dir)."""
    override = os.environ.get("AE_BIN")
    if override:
        return Path(override)
    name = "ae.exe" if os.name == "nt" else "ae"
    legacy = ROOT / "target" / "debug" / name
    if legacy.exists():
        return legacy
    try:
        import json

        metadata = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            capture_output=True, check=True, cwd=ROOT, text=True,
        )
        target_directory = json.loads(metadata.stdout)["target_directory"]
        return Path(target_directory) / "debug" / name
    except (OSError, ValueError, KeyError):
        return legacy


def corpus() -> list[Path]:
    paths: list[Path] = []
    for sub in ("tests/run", "examples"):
        directory = ROOT / sub
        if directory.is_dir():
            paths.extend(sorted(directory.glob("*.ae")))
    return paths


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--details", action="store_true", help="print every disagreement")
    args = parser.parse_args()

    exe = find_executable()
    if not exe.exists():
        print(f"ae binary not found at {exe}", file=sys.stderr)
        return 2

    env = {**os.environ, "AE_PURE_VERIFY": "1"}
    accepted = 0
    disagreements: list[str] = []
    for path in corpus():
        run = subprocess.run(
            [str(exe), "check", str(path)],
            capture_output=True, text=True, env=env,
        )
        if run.returncode != 0:
            # A rejected program is not this gate's business: the negative
            # corpora and the diagnostic-parity gate own those.
            continue
        accepted += 1
        for line in run.stderr.splitlines():
            if MARKER in line:
                disagreements.append(f"{path.relative_to(ROOT)}: {line}")
                break

    if args.details:
        for line in disagreements:
            print(line)
    print(f"programs={len(corpus())} accepted={accepted} disagreements={len(disagreements)}")
    return 1 if disagreements else 0


if __name__ == "__main__":
    sys.exit(main())