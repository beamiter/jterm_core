#!/usr/bin/env python3
"""Catalog and exact native evidence checks, including optimized Python."""
import copy
import importlib.util
import json
import argparse
import contextlib
import io
import os
import sys
import tempfile
from types import SimpleNamespace
from unittest.mock import patch
from pathlib import Path
import unittest

SCRIPT = Path(__file__).with_name("terminal-family-product-catalog.py")
SPEC = importlib.util.spec_from_file_location("product_catalog", SCRIPT)
catalog = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(catalog)
EVIDENCE_SPEC = importlib.util.spec_from_file_location(
    "product_evidence", SCRIPT.with_name("terminal-family-product-evidence.py"))
evidence = importlib.util.module_from_spec(EVIDENCE_SPEC)
EVIDENCE_SPEC.loader.exec_module(evidence)


class ProductCatalogTests(unittest.TestCase):
    def setUp(self):
        self.data = json.loads(catalog.CATALOG.read_text())

    def test_valid_catalog(self):
        catalog.validate(self.data)

    def test_runtime_promotion_is_rejected(self):
        row = self.data["journeys"][0]
        row["adapters"]["aether"]["runtime"] = "pass"
        with self.assertRaisesRegex(ValueError, "invalid runtime scope"):
            catalog.validate(self.data)
        row["adapters"]["aether"]["runtime"] = "not-run"
        row["test"]["status"] = "pass"
        with self.assertRaisesRegex(ValueError, "cannot award native pass"):
            catalog.validate(self.data)

    def test_duplicate_journey_is_rejected(self):
        self.data["journeys"].append(copy.deepcopy(self.data["journeys"][0]))
        with self.assertRaisesRegex(ValueError, "duplicate journey IDs"):
            catalog.validate(self.data)

    def test_unproven_target_must_have_work_owner(self):
        row = next(row for row in self.data["journeys"] if row["id"] == "PRODUCT.NOTEBOOK.001")
        row["target_adapters"].remove("frost")
        with self.assertRaisesRegex(ValueError, "unproven lane has no work target"):
            catalog.validate(self.data)

    def test_paths_cannot_fetch_outside_declared_objects(self):
        self.data["journeys"][0]["service"]["path"] = "../anything"
        with self.assertRaisesRegex(ValueError, "invalid repository-relative"):
            catalog.validate(self.data)

    def test_native_proof_requires_the_exact_one_success_receipt(self):
        digest = "a" * 64
        receipt = json.dumps({
            "case": evidence.AETHER_CLI, "backend": "x11", "result": "passed",
            "binary_sha256": digest,
        })
        def accepts(log, code=0, expected=digest):
            return evidence.named_native_result(log, evidence.AETHER_CLI, {"binary_sha256": expected}, code)
        self.assertTrue(accepts("driver diagnostic\n" + receipt))
        self.assertFalse(accepts(receipt, 1))
        self.assertFalse(accepts(receipt, expected="b" * 64))
        self.assertFalse(accepts(f"{receipt}\n{receipt}"))
        self.assertFalse(accepts('{"native_workspace_reopens_existing_provider_pty":true}'))

    def test_remote_proof_checks_both_artifacts(self):
        hashes = {"desktop_sha256": "a" * 64, "helper_sha256": "b" * 64}
        receipt = json.dumps({
            "case": evidence.AETHER_REMOTE, "backend": "x11", "result": "passed", **hashes,
        })
        self.assertTrue(evidence.named_native_result(receipt, evidence.AETHER_REMOTE, hashes, 0))
        self.assertFalse(evidence.named_native_result(
            receipt, evidence.AETHER_REMOTE, {**hashes, "helper_sha256": "c" * 64}, 0))

    def test_native_proof_does_not_promote_unrun_adapters_or_journeys(self):
        candidate = {"repositories": {name: {"commit": sha} for name, sha in self.data["repos"].items()}}
        rows = evidence.matrix(self.data, candidate, {evidence.AETHER_CLI: {"status": "native-pass"}})
        passed = [(row["test_id"], name) for row in rows for name, lane in row["adapters"].items()
                  if lane["status"] == "native-pass"]
        self.assertEqual(passed, [(evidence.AETHER_CLI, "aether")])
        self.assertEqual({name for row in rows for name in row["adapters"]}, catalog.ADAPTERS)


class ProductEvidenceTests(unittest.TestCase):
    def review_flags(self):
        return json.dumps(evidence.REVIEW_RECEIPT) + "\n" + json.dumps(evidence.UNAVAILABLE_RECEIPT)

    def test_review_x11_requires_complete_exact_once_boolean_witnesses(self):
        good = self.review_flags()
        accepts = lambda log, code=0: evidence.review_native_result(log, "x11", "a" * 64, code)
        self.assertTrue(accepts("diagnostic\n" + good))
        self.assertFalse(accepts(good, 1))
        self.assertFalse(accepts(json.dumps(evidence.REVIEW_RECEIPT)))
        self.assertFalse(accepts(good + "\n" + json.dumps(evidence.REVIEW_RECEIPT)))
        for value in [False, 1, "true"]:
            bad = dict(evidence.REVIEW_RECEIPT)
            bad[next(iter(bad))] = value
            self.assertFalse(accepts(json.dumps(bad) + "\n" + json.dumps(evidence.UNAVAILABLE_RECEIPT)))
        partial = dict(evidence.REVIEW_RECEIPT)
        partial.pop(next(iter(partial)))
        self.assertFalse(accepts(json.dumps(partial) + "\n" + json.dumps(evidence.UNAVAILABLE_RECEIPT)))
        self.assertFalse(accepts(json.dumps({**evidence.REVIEW_RECEIPT, "unrelated": True})
                                 + "\n" + json.dumps(evidence.UNAVAILABLE_RECEIPT)))
        self.assertFalse(evidence.named_native_result(good, evidence.AETHER_CLI, {"binary_sha256": "a" * 64}, 0))

    def test_review_wayland_requires_matching_named_file_log_and_artifact(self):
        digest = "a" * 64
        record = {"case_id": evidence.AETHER_REVIEW, "backend": "wayland", "result": "passed",
                  "binary_sha256": digest}
        with tempfile.TemporaryDirectory() as directory:
            receipt = Path(directory) / "result.json"
            log = json.dumps(record)
            receipt.write_text(log)
            accepts = lambda text=log, code=0, expected=digest: evidence.review_native_result(
                text, "wayland", expected, code, receipt)
            self.assertTrue(accepts())
            self.assertFalse(accepts(code=1))
            self.assertFalse(accepts(expected="b" * 64))
            self.assertFalse(accepts(log + "\n" + log))
            self.assertFalse(accepts(self.review_flags()))
            receipt.write_text(json.dumps({**record, "backend": "x11"}))
            self.assertFalse(accepts())
            receipt.write_text("{")
            self.assertFalse(accepts())
            receipt.unlink()
            self.assertFalse(accepts())
            saved = Path(directory) / "saved.json"
            saved.write_text(log)
            receipt.symlink_to(saved)
            self.assertFalse(accepts())

    def test_review_aggregate_requires_both_platforms_without_promoting_peers(self):
        passed = {"status": "native-pass"}
        self.assertEqual(evidence.review_observation({"x11": passed})["status"], "native-fail")
        self.assertEqual(evidence.review_observation({"x11": passed, "wayland": {"status": "native-fail"}})["status"], "native-fail")
        both = evidence.review_observation({"x11": passed, "wayland": passed})
        self.assertEqual(both["status"], "native-pass")
        data = json.loads(catalog.CATALOG.read_text())
        candidate = {"repositories": {name: {"commit": sha} for name, sha in data["repos"].items()}}
        rows = evidence.matrix(data, candidate, {evidence.AETHER_REVIEW: both})
        promoted = [(row["test_id"], name) for row in rows for name, lane in row["adapters"].items()
                    if lane["status"] == "native-pass"]
        self.assertEqual(promoted, [(evidence.AETHER_REVIEW, "aether")])

    def test_artifact_requires_regular_executable_elf_and_exact_digest(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "app"
            binary.write_bytes(b"\x7fELFtest fixture")
            binary.chmod(0o700)
            digest = evidence.sha256(binary)
            self.assertEqual(evidence.verify_artifact(binary, digest), binary)
            with self.assertRaisesRegex(ValueError, "differs"):
                evidence.verify_artifact(binary, "b" * 64)
            alias = Path(directory) / "alias"
            alias.symlink_to(binary)
            with self.assertRaisesRegex(ValueError, "regular"):
                evidence.verify_artifact(alias, digest)
            binary.write_text("#!/bin/sh\nexit 0\n")
            with self.assertRaisesRegex(ValueError, "not an ELF"):
                evidence.verify_artifact(binary, evidence.sha256(binary))
            binary.chmod(0o600)
            with self.assertRaisesRegex(ValueError, "regular"):
                evidence.verify_artifact(binary, evidence.sha256(binary))

    def test_runtime_tool_rejects_nonregular_input_before_hashing(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "pipe"
            os.mkfifo(path)
            with patch.object(evidence, "sha256") as digest:
                with self.assertRaisesRegex(ValueError, "regular executable"):
                    evidence.runtime_tool(path, "sway")
                digest.assert_not_called()

    def test_driver_requires_candidate_head_and_unchanged_git_blob(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            driver = root / "probe.py"
            driver.write_bytes(b"frozen")
            with patch.object(evidence.release_tool, "git", side_effect=[b"a" * 40 + b"\n", b"frozen"]):
                self.assertEqual(evidence.verify_driver(root, "a" * 40, "probe.py"), driver)
            with patch.object(evidence.release_tool, "git", return_value=b"b" * 40 + b"\n"):
                with self.assertRaisesRegex(ValueError, "native test checkout"):
                    evidence.verify_driver(root, "a" * 40, "probe.py")
            with patch.object(evidence.release_tool, "git", side_effect=[b"a" * 40 + b"\n", b"different"]):
                with self.assertRaisesRegex(ValueError, "differs from"):
                    evidence.verify_driver(root, "a" * 40, "probe.py")

    def test_run_native_isolates_platform_and_exact_vim_without_mutating_parent(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            vim = root / "actual-vim"
            vim.write_bytes(b"fixture")
            old_env = dict(os.environ)
            for backend in ("x11", "wayland"):
                def run(command, **kwargs):
                    env = kwargs["env"]
                    self.assertEqual("xvfb-run" in command, backend == "x11")
                    self.assertIn("240s", command)
                    self.assertNotIn("AT_SPI_BUS_ADDRESS", env)
                    for key in ("XDG_CACHE_HOME", "XDG_RUNTIME_DIR"):
                        path = Path(env[key])
                        self.assertEqual(path.stat().st_mode & 0o777, 0o700)
                    linked = Path(env["PATH"].split(os.pathsep)[0]) / "vim"
                    self.assertEqual(linked.resolve(), vim)
                    return SimpleNamespace(returncode=0)
                with patch.object(evidence.subprocess, "run", side_effect=run):
                    _, status = evidence.run_native(root / "driver.py", ["arg"], root / "log", backend=backend, vim=vim, seconds=240)
                self.assertEqual(status, 0)
            self.assertEqual(dict(os.environ), old_env)

    def test_review_inputs_fail_closed_when_incomplete_or_without_opt_in(self):
        defaults = dict(output_review=False, jsh=None, jsh_sha256=None, vim_binary=None, vim_sha256=None,
                        ime_client=None, ime_client_sha256=None, sway=None, wtype=None, compositor_libraries=None)
        parser = argparse.ArgumentParser()
        self.assertIsNone(evidence.review_inputs(SimpleNamespace(**defaults), parser, {}))
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                evidence.review_inputs(SimpleNamespace(**{**defaults, "jsh": Path("/app")}), parser, {})
            with self.assertRaises(SystemExit):
                evidence.review_inputs(SimpleNamespace(**{**defaults, "output_review": True}), parser, {})

    def test_main_preserves_old_invocation_and_records_both_review_lanes(self):
        for review, fail_wayland in [(False, False), (True, False), (True, True)]:
            with self.subTest(review=review, fail_wayland=fail_wayland), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / "driver.py").write_text("# frozen native driver fixture\n")
                app = root / "app"
                app.write_bytes(b"\x7fELFfixture")
                app.chmod(0o700)
                digest = evidence.sha256(app)
                candidate = root / "candidate.json"
                candidate.write_text(json.dumps({"repositories": {
                    name: {"commit": sha} for name, sha in json.loads(catalog.CATALOG.read_text())["repos"].items()}}))
                commits = {name: row["commit"] for name, row in json.loads(candidate.read_text())["repositories"].items()}
                output = root / "output"
                argv = ["tool", "--root", str(root), "--candidate", str(candidate), "--aether-binary", str(app),
                        "--aether-sha256", digest, "--output", str(output)]
                if review:
                    argv += ["--output-review", "--jsh", str(app), "--jsh-sha256", digest,
                             "--vim-binary", str(app), "--vim-sha256", digest,
                             "--ime-client", str(app), "--ime-client-sha256", digest]
                fixtures = {key: app for key in ("jsh", "vim_binary", "ime_client", "sway", "wtype", "wl_paste")}
                def native(driver, arguments, log, **kwargs):
                    if kwargs.get("backend") == "wayland":
                        receipt = {"case_id": evidence.AETHER_REVIEW, "backend": "wayland", "result": "passed", "binary_sha256": digest}
                        folder = Path(arguments[arguments.index("--output") + 1])
                        folder.mkdir()
                        (folder / "result.json").write_text(json.dumps(receipt))
                        log.write_text(json.dumps(receipt))
                        return ["fixture-wayland"], int(fail_wayland)
                    if "--output-review" in arguments:
                        self.assertIn("--vim-binary", arguments)
                        log.write_text(self.review_flags())
                    else:
                        self.assertIn("--workspace-only", arguments)
                        log.write_text(json.dumps({"case": evidence.AETHER_CLI, "backend": "x11", "result": "passed", "binary_sha256": digest}))
                    return ["fixture-x11"], 0
                with patch.object(sys, "argv", argv), patch.object(evidence, "verify_candidate", return_value=commits), \
                     patch.object(evidence, "verify_driver", return_value=root / "driver.py") as drivers, \
                     patch.object(evidence, "review_inputs", return_value=fixtures if review else None), \
                     patch.object(evidence, "run_native", side_effect=native), contextlib.redirect_stdout(io.StringIO()):
                    status = evidence.main()
                self.assertEqual(status, int(fail_wayland))
                rows = json.loads((output / "summary.json").read_text())["journeys"]
                lane = next(row for row in rows if row["test_id"] == evidence.AETHER_REVIEW)["adapters"]["aether"]
                self.assertEqual(lane["status"], "native-fail" if fail_wayland else "native-pass" if review else "not-run")
                if review:
                    self.assertEqual(set(lane["platform_results"]), {"x11", "wayland"})
                    self.assertEqual(sum(call.args[-1] == evidence.WAYLAND_DRIVER for call in drivers.call_args_list), 2)


if __name__ == "__main__":
    unittest.main()
