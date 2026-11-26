#!/usr/bin/env python3
"""Exercise false-pass cases in the family build-input gate."""
from copy import deepcopy
import json
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


class PublishedReleaseTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.record = {
            "schema": GATE["RELEASE_SCHEMA"],
            "applications": {},
            "acceptance": {"scope": "recorded runs are not replayed",
                           "native_product_matrix": {"status": "passed"}},
        }
        core_dir = self.init_repo("jterm_core")
        for path in ("Cargo.lock", "crates/jterm_engine/Cargo.lock"):
            self.write(core_dir, path, "version = 4\n")
        core, tree = self.commit(core_dir)
        self.record["shared_core"] = {
            "repository": "beamiter/jterm_core",
            "release_source_commit": core,
            "tree": tree,
            "validated_source_commit": core,
        }
        for name, locks in GATE["APP_LOCKS"].items():
            repo = self.init_repo(name)
            for path, packages in locks.items():
                manifest = Path(path).with_name("Cargo.toml")
                self.write(repo, manifest, "[dependencies]\n" + "".join(
                    f'{pkg} = {{ git = "{CORE}", rev = "{core}" }}\n'
                    for pkg in sorted(packages)
                ))
                self.write(repo, path, "version = 4\n" + "".join(
                    f'\n[[package]]\nname = "{pkg}"\nversion = "0.1.0"\n'
                    f'source = "git+{CORE}?rev={core}#{core}"\n'
                    for pkg in sorted(packages)
                ))
            release, tree = self.commit(repo)
            self.record["applications"][name] = {
                "repository": f"beamiter/{name}",
                "release_commit": release,
                "tree": tree,
                "validated_source_commit": "0" * 40,
                "core_pin": core,
            }

    def init_repo(self, name):
        repo = self.root / name
        repo.mkdir()
        self.git(repo, "init", "-q", "--object-format=sha1")
        return repo

    def git(self, repo, *args):
        return GATE["git"](repo, *args).decode().strip()

    def write(self, repo, path, data):
        path = repo / path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(data)

    def commit(self, repo):
        self.git(repo, "add", ".")
        self.git(repo, "-c", "user.name=Test", "-c", "user.email=test@example.invalid",
                 "-c", "commit.gpgsign=false", "commit", "-qm", "test release")
        return self.git(repo, "rev-parse", "HEAD"), self.git(repo, "rev-parse", "HEAD^{tree}")

    def cli(self, record, *extra):
        manifest = self.root / "release.json"
        manifest.write_text(json.dumps(record))
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--root", str(self.root), "--verify", str(manifest), *extra],
            capture_output=True, text=True, timeout=15,
        )

    def test_published_cli_verifies_immutable_inputs_without_asserting_native_pass(self):
        # The checker must read the recorded revision, not the live checkout.
        self.write(self.root / "forge", "Cargo.lock", "not TOML\n")
        self.commit(self.root / "forge")
        result = self.cli(self.record, "--require-common-core")
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["verification_scope"], "git_source_and_production_locks_only")
        self.assertEqual(report["production_lockfiles_verified"], 7)
        self.assertIs(report["recorded_acceptance_verified"], False)
        self.assertEqual(report["common_core_mismatches"], {})
        self.assertEqual(report["repositories"]["forge"]["validated_source_relation"],
                         {"availability": "not_available_locally"})
        self.assertTrue(report["repositories"]["jterm_core"]["validated_source_relation"]
                        ["equals_published_tree"])

    def test_published_cli_rejects_wrong_tree_wrong_claimed_pin_and_missing_app(self):
        for expected, change in (
            ("published tree differs", lambda c: c["applications"]["forge"].update(tree="b" * 40)),
            ("recorded app pin differs", lambda c: c["applications"]["forge"].update(core_pin="b" * 40)),
            ("must name all five", lambda c: c["applications"].pop("frost")),
        ):
            with self.subTest(expected=expected):
                case = deepcopy(self.record)
                change(case)
                result = self.cli(case)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertIn(expected, result.stderr)

    def test_rejects_consistent_manifest_and_lock_pinning_the_wrong_commit(self):
        case = deepcopy(self.record)
        other = "b" * 40
        repo = self.root / "forge"
        for path in ("Cargo.toml", "Cargo.lock"):
            disk = repo / path
            disk.write_text(disk.read_text().replace(case["shared_core"]["release_source_commit"], other))
        commit, tree = self.commit(repo)
        case["applications"]["forge"].update(release_commit=commit, tree=tree)
        result = self.cli(case)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("published locks disagree with the shared core: forge/Cargo.lock", result.stderr)

    def test_legacy_snapshot_verifies_exactly_as_before(self):
        old = GATE["snapshot"](self.root)
        result = self.cli(old, "--require-common-core")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), old)


if __name__ == "__main__":
    unittest.main()
