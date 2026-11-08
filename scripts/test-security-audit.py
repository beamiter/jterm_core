#!/usr/bin/env python3
"""Offline tests for the pinned cargo-audit 0.22.2 terminal diagnostic guard."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
LOCKS = ["Cargo.lock", "tools/companion-host-harness/Cargo.lock", "crates/jterm_engine/Cargo.lock"]
DIAGNOSTICS = [
    "error: couldn't check if the package is yanked: missing crate",
    "warning: couldn't update crates.io index: offline",
    "warning: couldn't open crates.io index: permission denied",
]
FAKE = """#!/usr/bin/env python3
import json, os, sys
a = sys.argv[1:]
s = json.loads(os.environ["SCENARIO"])
if a == ["audit", "--version"]:
    print(s.get("version", "cargo-audit 0.22.2"))
    sys.exit(s.get("version_status", 0))
with open(os.environ["CALLS"], "a") as f:
    f.write(json.dumps(a) + "\\n")
lock = a[a.index("--file") + 1]
print("Scanning " + lock)
if lock == s.get("lock"):
    print(s.get("text", ""), file=getattr(sys, s.get("stream", "stderr")))
    sys.exit(s.get("status", 0))
"""
FAKE_GREP = r'''#!/usr/bin/env python3
import json, os, subprocess, sys
data = sys.stdin.read()
s = json.loads(os.environ["SCENARIO"])
if "Scanning " + s["lock"] + "\n" in data:
    sys.exit(s["grep_status"])
sys.exit(subprocess.run([os.environ["REAL_GREP"], *sys.argv[1:]],
                        input=data, text=True).returncode)
'''
class Gate(unittest.TestCase):
    def run_gate(self, scenario=None):
        with tempfile.TemporaryDirectory(prefix="core-audit-test-") as directory:
            root = Path(directory)
            (root / "scripts").mkdir()
            (root / "bin").mkdir()
            shutil.copyfile(ROOT / "scripts/security-audit.sh", root / "scripts/security-audit.sh")
            (root / "bin/cargo").write_text(FAKE)
            (root / "bin/cargo").chmod(0o700)
            scenario = scenario or {}
            if "grep_status" in scenario:
                (root / "bin/grep").write_text(FAKE_GREP)
                (root / "bin/grep").chmod(0o700)
            if "config" in scenario:
                (root / ".cargo").mkdir()
                (root / ".cargo/audit.toml").write_text(scenario["config"])
            calls = root / "calls"
            env = dict(os.environ, PATH=str(root / "bin") + ":" + os.environ["PATH"],
                       SCENARIO=json.dumps(scenario), CALLS=str(calls), CARGO_TERM_COLOR="always",
                       REAL_GREP=shutil.which("grep"))
            result = subprocess.run(["bash", str(root / "scripts/security-audit.sh")],
                                    env=env, capture_output=True, text=True)
            rows = [json.loads(x) for x in calls.read_text().splitlines()] if calls.exists() else []
            for row in rows:
                self.assertEqual(row, ["audit", "--format", "terminal", "--color", "never",
                                       "--deny", "warnings", "--file", row[-1]])
                for flag in ("--no-fetch", "--no-yanked", "--quiet", "--stale", "--json", "--ignore"):
                    self.assertNotIn(flag, row)
            return result, [row[-1] for row in rows]

    def test_clean(self):
        r, calls = self.run_gate()
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(calls, LOCKS)

    def test_incomplete_checks(self):
        for lock in LOCKS:
            for diagnostic in DIAGNOSTICS:
                for stream in ("stdout", "stderr"):
                    with self.subTest(lock=lock, diagnostic=diagnostic, stream=stream):
                        r, calls = self.run_gate(dict(lock=lock, text=diagnostic, stream=stream))
                        self.assertEqual(r.returncode, 1)
                        self.assertIn(diagnostic, r.stdout)
                        self.assertIn("incomplete yanked-crate check", r.stderr)
                        self.assertEqual(calls, LOCKS[:LOCKS.index(lock)+1])

    def test_upstream_findings_and_failures_preserved(self):
        for lock in LOCKS:
            for status in (1, 2, 37):
                for finding in ("vulnerability", "unmaintained", "unsound", "notice", "yanked"):
                    with self.subTest(lock=lock, status=status, finding=finding):
                        r, calls = self.run_gate(dict(lock=lock, text=finding, status=status))
                        self.assertEqual(r.returncode, status)
                        self.assertEqual(calls, LOCKS[:LOCKS.index(lock)+1])

    def test_checker_failure_and_upstream_precedence(self):
        for lock in LOCKS:
            for status in (2, 127):
                with self.subTest(lock=lock, status=status):
                    r, calls = self.run_gate(dict(lock=lock, grep_status=status))
                    self.assertEqual(r.returncode, status)
                    self.assertIn("could not validate audit diagnostics", r.stderr)
                    self.assertEqual(calls, LOCKS[:LOCKS.index(lock)+1])
                    r, calls = self.run_gate(dict(grep_status=status, lock=lock, status=37))
                    self.assertEqual(r.returncode, 37)
                    self.assertNotIn("could not validate audit diagnostics", r.stderr)
                    self.assertEqual(calls, LOCKS[:LOCKS.index(lock)+1])

    def test_unreviewed_config_is_rejected_before_audit(self):
        for config in ("[output]\nquiet = true\n", "[yanked]\nenabled = false\n",
                       "[database]\nfetch = false\n", ""):
            with self.subTest(config=config):
                r, calls = self.run_gate(dict(config=config))
                self.assertEqual(r.returncode, 1)
                self.assertIn("requires revalidation", r.stderr)
                self.assertEqual(calls, [])

    def test_nearby_text_is_not_infrastructure_failure(self):
        for lock in LOCKS:
            r, calls = self.run_gate(dict(lock=lock, text="\n".join("note: " + d for d in DIAGNOSTICS)))
            self.assertEqual(r.returncode, 0)
            self.assertEqual(calls, LOCKS)

    def test_official_release_binary_version_label(self):
        r, calls = self.run_gate(dict(version="cargo-audit-audit 0.22.2"))
        self.assertEqual(r.returncode, 0)
        self.assertEqual(calls, LOCKS)

    def test_tool_version_is_pinned(self):
        for scenario, status in ((dict(version="cargo-audit 0.23.0"), 1),
                                 (dict(version_status=42), 42)):
            r, calls = self.run_gate(scenario)
            self.assertEqual(r.returncode, status)
            self.assertEqual(calls, [])

    def test_workflow_contract(self):
        workflow = (ROOT / ".github/workflows/security.yml").read_text()
        self.assertIn('CARGO_AUDIT_VERSION: "0.22.2"', workflow)
        for path in ("Cargo.toml", *LOCKS, "tools/companion-host-harness/Cargo.toml", "crates/jterm_engine/Cargo.toml",
                     "scripts/security-audit.sh", "scripts/test-security-audit.py",
                     ".github/workflows/security.yml", ".cargo/audit.toml"):
            self.assertEqual(workflow.count("      - " + path + "\n"), 2, path)
        self.assertIn("run: bash scripts/security-audit.sh", workflow)
        self.assertLess(workflow.index("run: python3 scripts/test-security-audit.py"),
                        workflow.index("run: bash scripts/security-audit.sh"))
        # No quiet/config override is introduced by this minimal candidate.
        self.assertFalse((ROOT / ".cargo/audit.toml").exists())

if __name__ == "__main__":
    unittest.main(verbosity=2)
