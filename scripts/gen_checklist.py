"""Generate the master checklist from the implementation ledger (R5.1).

The ledger (`docs/implementation-ledger.md`) is the single source of truth:
every `- [x]` / `- [ ] ID. Title` line becomes one checklist line under the
section it belongs to. The generated file is deterministic and is verified
against the ledger by `scripts/test_gen_checklist.py` and by a CI step, so
the checklist is never edited by hand.

Usage: python scripts/gen_checklist.py [--ledger PATH] [--out PATH]
"""

import argparse
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LEDGER = ROOT / "docs" / "implementation-ledger.md"
CHECKLIST = ROOT / "docs" / "checklist.md"

ITEM = re.compile(r"^- \[( |x)\] (\S+)[.:] (.*)$")
# A prototype-done item that is still short of Charter-1.0 semantics carries
# an explicit marker naming the phase that closes the gap (audit §8):
#   [1.0-gap: P2.2]
# The checklist then reports two progress numbers instead of one, so
# 70%-on-paper cannot be mistaken for 70%-toward-1.0 (Hard Rule 10).
GAP = re.compile(r"\[1\.0-gap:\s*([^\]]+)\]")


def parse_items(ledger_text: str):
    """Yield (section, done, item_id, first_line, wrapped, gap) per ledger item.

    An item is a `- [ ]` / `- [x]` line; continuation lines belong to the
    item's body in the ledger and are not mirrored here — the checklist
    carries the ID and the first line only.
    """
    section = ""
    for line in ledger_text.split("\n"):
        if line.startswith("## "):
            section = line[3:].strip()
            continue
        match = ITEM.match(line)
        if match:
            done, item_id, title = match.groups()
            wrapped = not line.rstrip().endswith((".", ":", "）", ")"))
            gap = GAP.search(title)
            yield section, done == "x", item_id.rstrip(".:"), title, wrapped, gap.group(1).strip() if gap else None


def render(ledger_text: str) -> str:
    """The full checklist document for the given ledger text."""
    items = list(parse_items(ledger_text))
    done_count = sum(1 for _, done, _, _, _, _ in items if done)
    # Charter-1.0 progress: a prototype-done item whose marker names a gap
    # phase does not count as 1.0-done. Two numbers keep the report honest.
    gap_count = sum(1 for _, done, _, _, _, gap in items if done and gap)
    charter_done = done_count - gap_count
    out = []
    out.append("# ae master checklist")
    out.append("")
    out.append("Generated from `docs/implementation-ledger.md` by `scripts/gen_checklist.py` "
               "(R5.1): do not edit by hand. Item bodies, evidence links and progress notes")
    out.append("live in the ledger; this file mirrors only the checkbox state and titles.")
    out.append("")
    out.append(f"Prototype progress: {done_count} of {len(items)} items done.")
    gap_note = ("1" if gap_count == 1 else "s")
    out.append(f"Charter-1.0 progress: {charter_done} of {len(items)} items done "
               f"({gap_count} prototype-done item{gap_note} still name an open "
               f"gap, marked `[1.0-gap: …]`).")
    out.append("")
    current_section = None
    for section, done, item_id, title, wrapped, gap in items:
        if section != current_section:
            if current_section is not None:
                out.append("")
            out.append(f"## {section}")
            out.append("")
            current_section = section
        marker = "x" if done else " "
        suffix = " …" if wrapped else ""
        out.append(f"- [{marker}] {item_id}: {title}{suffix}")
    out.append("")
    return "\n".join(out)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ledger", default=str(LEDGER))
    parser.add_argument("--out", default=str(CHECKLIST))
    options = parser.parse_args()
    ledger_text = Path(options.ledger).read_text(encoding="utf-8")
    Path(options.out).write_text(render(ledger_text), encoding="utf-8", newline="\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
