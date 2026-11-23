#!/usr/bin/env python3
"""Metadata and capture-control regression tests; no Vim or PTY is started."""
from collections import deque
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

from capture_diagnostics import CaptureInputEvidence, capture_failure_metadata
from test_bridge import Editor
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
        self.assertNotIn("pty_write_acceptance", str(error.exception))
        self.editor.ex.assert_called_once()
        self.editor.pump.assert_not_called()

    def test_capture_timeout_reports_fixed_input_phases_without_replaying(self):
        evidence = CaptureInputEvidence()
        editor = Editor.__new__(Editor)
        editor.master = 42
        editor.messages = deque()
        editor.state = {"mode": "n"}
        editor.connection = object()
        editor.process = self.editor.process
        editor.state_where = Mock()
        harness = SimpleNamespace(root=self.root, fail=self.fail)
        expression = "PRIVATE_CAPTURE_é"
        command = (":call writefile([json_encode(" + expression + ")], "
                   + workbench.vim_string(self.root / "capture-1.json") + ")").encode()

        with patch("test_bridge.os.write", side_effect=[5, len(command) - 1, 1]) as write:
            editor.keys("gg0y$", observe_write=evidence.observe_yank)
            with patch("test_vimrc_workbench.time.monotonic", side_effect=[100, 106]):
                with self.assertRaisesRegex(AssertionError, "metadata=") as error:
                    workbench.VimrcWorkbenchTests.capture(
                        harness, editor, expression, input_evidence=evidence)

        self.assertEqual([call.args for call in write.call_args_list],
                         [(42, b"gg0y$"), (42, command), (42, b"\r")])
        wait_predicates = [call.args[0] for call in editor.state_where.call_args_list]
        self.assertEqual(len(wait_predicates), 2)
        self.assertTrue(wait_predicates[0]({"mode": "c"}))
        self.assertFalse(wait_predicates[0]({"mode": "n"}))
        self.assertTrue(wait_predicates[1]({"mode": "n"}))
        self.assertFalse(wait_predicates[1]({"mode": "c"}))
        text = str(error.exception)
        self.assertNotIn("PRIVATE_CAPTURE", text)
        self.assertNotIn("capture-1.json", text)
        self.assertLess(len(text), 512)
        metadata = json.loads(text.split("metadata=", 1)[1])
        self.assertEqual(metadata["pty_write_acceptance"],
                         {"yank": "complete", "command": "partial", "enter": "complete"})

    def test_pty_evidence_uses_closed_status_vocabulary(self):
        evidence = CaptureInputEvidence()
        self.assertEqual(evidence.metadata(),
                         {"yank": "unobserved", "command": "unobserved", "enter": "unobserved"})
        evidence.observe_yank(5, 0)
        evidence.observe_command(40, "PRIVATE" * 10000)
        evidence.observe_enter(True, 1)
        self.assertEqual(evidence.metadata(),
                         {"yank": "none", "command": "invalid", "enter": "invalid"})

    def test_success_with_pty_evidence_never_serializes_it(self):
        harness = SimpleNamespace(root=self.root, fail=self.fail)
        evidence = Mock(spec=CaptureInputEvidence)
        self.editor.ex.side_effect = lambda *_args, **_kwargs: (
            self.root / "capture-1.json").write_text("42")
        self.assertEqual(workbench.VimrcWorkbenchTests.capture(
            harness, self.editor, "42", input_evidence=evidence), 42)
        evidence.metadata.assert_not_called()

    def test_capture_success_does_not_collect_diagnostics(self):
        harness = SimpleNamespace(root=self.root, fail=self.fail)
        self.editor.ex.side_effect = lambda _: (self.root / "capture-1.json").write_text("42")
        with patch("test_vimrc_workbench.capture_failure_metadata") as diagnostic:
            self.assertEqual(workbench.VimrcWorkbenchTests.capture(harness, self.editor, "42"), 42)
        diagnostic.assert_not_called()
        self.editor.ex.assert_called_once()


if __name__ == "__main__":
    unittest.main()
