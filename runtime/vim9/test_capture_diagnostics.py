#!/usr/bin/env python3
"""Metadata and capture-control regression tests; no Vim or PTY is started."""
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

from capture_diagnostics import capture_failure_metadata
import test_vimrc_workbench as workbench


class CaptureDiagnosticsTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.output = self.root / "private-name.json"
        self.editor = SimpleNamespace(
            state={"mode": "n", "window": 1000, "buffer": 2, "changedtick": 3},
            connection=object(), process=Mock(), ex=Mock(), pump=Mock())
        self.editor.process.poll.return_value = None

    def test_missing_result_and_running_process(self):
        data = capture_failure_metadata(self.editor, self.output)
        self.assertEqual(data["result_kind"], "missing")
        self.assertEqual(data["process_status"], "running")
        self.assertEqual(data["mode"], "n")
        self.assertTrue(data["bridge_connected"])
        self.editor.ex.assert_not_called()
        self.editor.pump.assert_not_called()

    def test_existing_result_reports_only_bounded_size(self):
        self.output.write_text("SECRET CONTENT")
        data = capture_failure_metadata(self.editor, self.output)
        self.assertEqual(data["result_bytes_capped"], 14)
        self.assertNotIn("SECRET", json.dumps(data))
        with self.output.open("wb") as stream:
            stream.truncate(2 * 1024 * 1024)
        self.assertEqual(capture_failure_metadata(self.editor, self.output)[
            "result_bytes_capped"], 1024 * 1024)

    def test_symlink_is_not_followed(self):
        self.output.symlink_to(self.root / "missing-private-target")
        data = capture_failure_metadata(self.editor, self.output)
        self.assertEqual(data["result_kind"], "other")
        self.assertNotIn("result_bytes_capped", data)

    def test_untrusted_values_cannot_expand_or_leak(self):
        secret = "PRIVATE" * 10000
        self.editor.state = {"mode": secret, "window": secret,
                             "buffer": True, "changedtick": 2**1000,
                             "text": secret, "path": secret, "token": secret}
        text = json.dumps(capture_failure_metadata(self.editor, self.output))
        self.assertNotIn("PRIVATE", text)
        self.assertLess(len(text), 512)

    def test_exited_disconnected_and_unavailable_metadata(self):
        self.editor.process.poll.return_value = -9
        self.editor.connection = None
        data = capture_failure_metadata(self.editor, self.output)
        self.assertEqual((data["process_status"], data["exit_code"]), ("exited", -9))
        self.assertFalse(data["bridge_connected"])
        self.editor.process.poll.side_effect = OSError("PRIVATE")
        output = Mock()
        output.lstat.side_effect = PermissionError("PRIVATE")
        data = capture_failure_metadata(self.editor, output)
        self.assertEqual(data["process_status"], "unavailable")
        self.assertEqual(data["result_kind"], "unavailable")
        self.assertNotIn("PRIVATE", json.dumps(data))

    def test_capture_timeout_adds_no_commands_or_extra_wait(self):
        harness = SimpleNamespace(root=self.root, fail=self.fail)
        with patch("test_vimrc_workbench.time.monotonic", side_effect=[100, 106]):
            with self.assertRaisesRegex(AssertionError, "metadata=") as error:
                workbench.VimrcWorkbenchTests.capture(harness, self.editor, "PRIVATE_EXPRESSION")
        self.assertNotIn("PRIVATE_EXPRESSION", str(error.exception))
        self.editor.ex.assert_called_once()
        self.editor.pump.assert_not_called()

    def test_capture_success_does_not_collect_diagnostics(self):
        harness = SimpleNamespace(root=self.root, fail=self.fail)
        self.editor.ex.side_effect = lambda _: (self.root / "capture-1.json").write_text("42")
        with patch("test_vimrc_workbench.capture_failure_metadata") as diagnostic:
            self.assertEqual(workbench.VimrcWorkbenchTests.capture(harness, self.editor, "42"), 42)
        diagnostic.assert_not_called()
        self.editor.ex.assert_called_once()


if __name__ == "__main__":
    unittest.main()
