#!/usr/bin/env python3
"""Exercise false-pass cases in the family build-input gate."""
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("terminal-family-release.py")
GATE = runpy.run_path(str(SCRIPT))
REVISION = "a" * 40
CORE = GATE["CORE_URL"]


class ReleaseGateTests(unittest.TestCase):
    def setUp(self):
        self.manifest = {"dependencies": {"jterm_engine": {"git": CORE, "rev": REVISION}}}
        self.package = {"name": "jterm_engine", "source": f"git+{CORE}.git?rev={REVISION}#{REVISION}"}
        self.lock = {"package": [self.package]}

    def pins(self):
        return GATE["app_core_pins"](self.manifest, self.lock, {"jterm_engine"}, "fixture")

    def test_manifest_and_lock_agree_with_or_without_git_url_suffix(self):
        self.assertEqual(self.pins(), {"jterm_engine": REVISION})

    def test_path_dependency_is_refused_even_with_a_matching_git_lock(self):
        self.manifest["dependencies"]["jterm_engine"]["path"] = "../local"
        with self.assertRaisesRegex(ValueError, "manifest dependency disagrees"):
            self.pins()

    def test_unpinned_or_conflicting_lock_entries_are_refused(self):
        self.package.pop("source")
        with self.assertRaisesRegex(ValueError, "one full Git pin"):
            self.pins()
        other = "b" * 40
        self.package["source"] = f"git+{CORE}?rev={REVISION}#{other}"
        with self.assertRaisesRegex(ValueError, "one full Git pin"):
            self.pins()

    def test_capture_uses_explicit_revisions_without_reading_checkout_heads(self):
        commits = {name: REVISION for name in GATE["REPOSITORIES"]}
        arguments = [f"{name}={revision}" for name, revision in commits.items()]
        self.assertEqual(GATE["capture_commits"](Path("nonexistent"), arguments), commits)
        for invalid in ("aether=master", "aether=abcd", f"unknown={REVISION}"):
            with self.subTest(invalid=invalid), self.assertRaisesRegex(ValueError, "REPOSITORY=FULL_SHA"):
                GATE["capture_commits"](Path("nonexistent"), [invalid])
        with self.assertRaisesRegex(ValueError, "specified twice"):
            GATE["capture_commits"](Path("nonexistent"), arguments + arguments[:1])

    def test_null_verification_file_never_captures_current_checkouts(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "release.json"
            path.write_text("null")
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "--verify", str(path)],
                capture_output=True, text=True, timeout=10,
            )
        self.assertEqual(result.returncode, 1)
        self.assertIn("expected a schema 1 family release record", result.stderr)
        self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
