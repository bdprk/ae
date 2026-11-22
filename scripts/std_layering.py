"""Check that standard-library imports resolve and only point down the layer stack."""

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STD = ROOT / "std"

USE = re.compile(r'^\s*use\s+"([^"]+)"\s*;')

# Compiler models may use allocation helpers. Hosted modules sit above them.
LAYERS = {"core": 0, "alloc": 1, "compiler": 2, "std": 3}
DEFAULT_RANK = LAYERS["std"]


def layer_of(relative: Path) -> int:
    """Rank of a std file from its path below std/ (its first directory)."""
    parts = relative.parts
    if len(parts) >= 2:
        return LAYERS.get(parts[0], DEFAULT_RANK)
    return DEFAULT_RANK


def header_uses(source: str):
    for line in source.split("\n"):
        match = USE.match(line)
        if match:
            yield match.group(1)


def check(std_root: Path = STD):
    """Return every layering violation as a human-readable line."""
    std_root = std_root.resolve()
    violations = []
    files = sorted(std_root.rglob("*.ae"))
    for file in files:
        relative = file.relative_to(std_root)
        source = file.read_text(encoding="utf-8")
        for use_path in header_uses(source):
            target = (file.parent / use_path).resolve()
            if not target.exists():
                violations.append(f"{relative}: use \"{use_path}\" does not resolve")
                continue
            if not target.is_relative_to(std_root):
                violations.append(f'{relative}: use "{use_path}" escapes the std tree')
                continue
            imported_relative = target.relative_to(std_root)
            if layer_of(relative) < layer_of(imported_relative):
                violations.append(
                    f"{relative}: {relative.parts[0]} imports higher layer "
                    f"{imported_relative.as_posix()}"
                )
    return violations


def main() -> int:
    violations = check()
    if violations:
        print(f"{len(violations)} std layering violations:", file=__import__("sys").stderr)
        for violation in violations:
            print(f"  {violation}", file=__import__("sys").stderr)
        return 1
    print("std layering holds")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
