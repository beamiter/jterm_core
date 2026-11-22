#!/usr/bin/env python3
"""Ensure skipped or wrong Rust tests cannot become acceptance evidence."""
import importlib.util
from pathlib import Path
import unittest

SCRIPT = Path(__file__).with_name("terminal-family-evidence.py")
SPEC = importlib.util.spec_from_file_location("family_evidence", SCRIPT)
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


class EvidenceTests(unittest.TestCase):
    def test_exactly_one_pass(self):
        output = ("running 1 test\ntest case_a ... ok\n\n"
                  "test result: ok. 1 passed; 0 failed; 0 ignored; 99 filtered out\n")
        self.assertTrue(runner.passed_once(output, "case_a", 0))
        for bad, name, rc in [
            (output, "case_b", 0),
            (output, "case_a", 101),
            (output.replace("running 1 test", "running 2 tests"), "case_a", 0),
            (output.replace("... ok", "... ignored"), "case_a", 0),
            ("running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored;", "case_a", 0),
        ]:
            with self.subTest(bad=bad, name=name, rc=rc):
                self.assertFalse(runner.passed_once(bad, name, rc))

    def test_checked_in_catalog_identifies_five_without_promoting_them(self):
        data = runner.catalog(SCRIPT.resolve().parents[1])
        self.assertEqual(set(data["frontends"]), runner.ADAPTERS)
        self.assertTrue(any(case["oracle"] == "literal" for case in data["cases"]))
        self.assertTrue(any(case["oracle"] == "historical-differential" for case in data["cases"]))


if __name__ == "__main__":
    unittest.main()
