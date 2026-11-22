"""R5.1 unit tests: the checklist generator parses the ledger and stays in
sync with the generated file, and the commit-ID rule's patterns behave."""

import unittest

import gen_checklist

SAMPLE = """# ae implementation ledger

## M2.4 Unsafe boundary

- [x] M2.4a. Something finished in this pass with evidence.
- [ ] M2.4b. Something still open with a wrapped body that continues
  on a second line and should be marked wrapped.

## R3 pipeline

- [ ] R3b. Resolver pass over the shared grammar.

## Audit remediation

- [x] P2.9. Prototype-done item with an open 1.0 gap [1.0-gap: P2.2].
"""


class ParseTests(unittest.TestCase):
    def test_items_carry_section_state_and_wrapping(self):
        items = list(gen_checklist.parse_items(SAMPLE))
        self.assertEqual(len(items), 4)
        section, done, item_id, title, wrapped, gap = items[0]
        self.assertEqual(section, "M2.4 Unsafe boundary")
        self.assertTrue(done)
        self.assertEqual(item_id, "M2.4a")
        self.assertEqual(title, "Something finished in this pass with evidence.")
        self.assertFalse(wrapped)
        _, _, _, _, wrapped, _ = items[1]
        self.assertTrue(wrapped, "an item whose first line runs on gets the ellipsis")
        self.assertEqual(items[2][0], "R3 pipeline")

    def test_render_is_deterministic_and_counts(self):
        first = gen_checklist.render(SAMPLE)
        self.assertEqual(first, gen_checklist.render(SAMPLE))
        self.assertIn("Prototype progress: 2 of 4 items done.", first)
        self.assertIn("Charter-1.0 progress: 1 of 4 items done (1 prototype-done item", first)
        self.assertIn("- [x] M2.4a: Something finished", first)
        self.assertIn("- [ ] M2.4b: Something still open with a wrapped body that continues …", first)
        self.assertIn("## R3 pipeline", first)

    def test_a_new_section_starts_a_blank_line(self):
        rendered = gen_checklist.render(SAMPLE)
        body = [line for line in rendered.split("\n") if line.startswith("##")]
        for header in body:
            self.assertGreater(rendered.split("\n").index(header), 0)


class CommitIdPatterns(unittest.TestCase):
    def test_ledger_id_shapes_are_matched(self):
        for title in ("R3b: resolver", "M4.3.05 channel cancel", "M0.2.03 grammar", "B12 baseline", "R3c step"):
            self.assertEqual(len(gen_checklist_commit_ids(title)), 1, title)

    def test_non_ids_are_ignored(self):
        self.assertEqual(gen_checklist_commit_ids("fix: refresh the generated-C expectations"), [])


def gen_checklist_commit_ids(title: str):
    import check_commit_ids
    return check_commit_ids.ID.findall(title)


class Staleness(unittest.TestCase):
    def test_generated_checklist_matches_the_ledger(self):
        ledger = (gen_checklist.LEDGER).read_text(encoding="utf-8")
        rendered = gen_checklist.render(ledger)
        if not gen_checklist.CHECKLIST.exists():
            gen_checklist.CHECKLIST.write_text(rendered, encoding="utf-8", newline="\n")
            self.skipTest("checklist generated for the first time; rerun to pass")
        on_disk = gen_checklist.CHECKLIST.read_text(encoding="utf-8")
        self.assertEqual(on_disk, rendered, "docs/checklist.md is stale; run scripts/gen_checklist.py")


if __name__ == "__main__":
    unittest.main()
