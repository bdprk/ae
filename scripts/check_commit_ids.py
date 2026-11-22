"""Check that commit titles record their work in the ledger (R5.1).

Rule: when a commit title names ledger item IDs (`R3b: resolver …`,
`M4.3.05 …`), the commit must touch `docs/implementation-ledger.md` —
a flip (`[ ]` -> `[x]`), a progress note on the item, anything: the
discipline is that claimed work is recorded where the ledger lives, in the
same commit. A commit that claims IDs but never edits the ledger fails.
Commits without IDs in the title are skipped; merge commits are skipped.
Whether a claimed ID was also *flipped* is reported informationally: R5.1's
flipping rule applies to completion commits, not to progress notes.

The range defaults to `@{u}..HEAD` (what a push would publish); pass
`--base <ref>` to check anything else. CI can wire this once the workflow
fetches full history; locally it runs against the real repository.

Usage: python scripts/check_commit_ids.py [--base REF] [--ledger PATH]
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LEDGER = ROOT / "docs" / "implementation-ledger.md"

# Ledger IDs look like `B01`, `R3a`, `R3b`, `M4.3.05`, `M0.2.03`, `M2.13`.
ID = re.compile(r"(?<![A-Za-z0-9])([A-Z]+\d+(?:\.\d+)*(?:[a-z])?|[A-Z]+\d+[a-z])(?![A-Za-z0-9])")


def run_git(args: list[str]) -> str:
    result = subprocess.run(["git", *args], capture_output=True, encoding="utf-8", check=True, cwd=ROOT)
    return result.stdout


def commits(base: str) -> list[tuple[str, str]]:
    """(hash, title) for every non-merge commit in base..HEAD, oldest first."""
    lines = run_git(["log", "--format=%H%x00%s", "--no-merges", f"{base}..HEAD"])
    out = []
    for line in lines.split("\n"):
        if not line:
            continue
        commit_hash, title = line.split("\x00", 1)
        out.append((commit_hash, title))
    return out


def docs_only(commit_hash: str) -> bool:
    """Whether the commit changes nothing outside `docs/`.

    A docs commit's title may describe state in passing ("handoff: R4.1
    done") — that is a summary of the ledger, not a claim of completed work
    that must edit it in the same commit. Code commits keep the strict rule.
    """
    listing = run_git(["show", "--format=", "--name-only", commit_hash])
    files = [name for name in listing.splitlines() if name.strip()]
    return bool(files) and all(name.startswith("docs/") for name in files)


def touches_ledger(commit_hash: str) -> bool:
    """Whether the commit changed the ledger file at all."""
    files = run_git(["show", "--format=", "--name-only", commit_hash, "--", LEDGER.relative_to(ROOT).as_posix()])
    return bool(files.strip())


def flipped_ids(commit_hash: str) -> set[str]:
    """The ledger IDs this commit checked off (`[ ]` -> `[x]`), if any."""
    diff = run_git(["show", "--format=", "--unified=0", commit_hash, "--", LEDGER.relative_to(ROOT).as_posix()])
    unchecked = set()
    checked = set()
    for line in diff.splitlines():
        if line.startswith("---") or line.startswith("+++"):
            continue
        entry = line[1:].lstrip()
        if line.startswith("-") and entry.startswith("- [ ]"):
            items = unchecked
        elif line.startswith("+") and entry.startswith("- [x]"):
            items = checked
        else:
            continue
        match = ID.search(entry[5:])
        if match:
            items.add(match.group(1))
    return unchecked & checked


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="@{u}")
    parser.add_argument("--ledger", default=str(LEDGER))
    options = parser.parse_args()
    violations = []
    for commit_hash, title in commits(options.base):
        claimed = set(ID.findall(title))
        if not claimed:
            continue
        if docs_only(commit_hash):
            continue
        if touches_ledger(commit_hash):
            flipped = flipped_ids(commit_hash)
            if claimed - flipped:
                note = "progress recorded without a flip" if flipped else "ledger edited without flipping the claimed items"
                print(f"note: {commit_hash[:8]} {title!r} claims {sorted(claimed)}: {note}")
            continue
        violations.append(f"{commit_hash[:8]} {title!r} claims {sorted(claimed)} but never touches the ledger")
    if violations:
        print(f"{len(violations)} commit(s) claim ledger items they do not record:", file=sys.stderr)
        for violation in violations:
            print(f"  {violation}", file=sys.stderr)
        return 1
    print("commit titles and ledger records agree")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
