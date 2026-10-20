#!/usr/bin/env python3
"""Versioned row paint transport exercised against an actual Vim PTY.

Run: python3 runtime/vim9/test_render_delta.py
The editable document always lives in Vim. This receiver reconstructs paint
frames only, to compare real delta output with an explicit Vim keyframe.
"""

from __future__ import annotations

import copy
import json
from pathlib import Path
import tempfile
import time
import unittest

from test_bridge import Editor, VIM


def canonical(frame):
    result = copy.deepcopy(frame)
    for key in ("event", "revision", "protocol", "seq"):
        result.pop(key, None)
    result["runs"].sort(key=lambda run: run["row"])
    return result


class DeltaEditor(Editor):
    def __init__(self, *args, **kwargs):
        self.paint = None
        self.paint_seq = 0
        self.paint_events = []
        super().__init__(*args, **kwargs)

    def authenticate(self):
        self.send({"action": "auth", "token": self.token, "protocol": 1,
                   "render_protocol": 2})
        return self.wait(lambda message: message.get("event") == "state")

    def pump(self, timeout):
        super().pump(timeout)
        for message in tuple(self.messages):
            if (message.get("event") not in ("render", "render_delta")
                    or message.get("seq", 0) <= self.paint_seq):
                continue
            self.paint_seq = message["seq"]
            self.paint_events.append(copy.deepcopy(message))
            if message["event"] == "render":
                self.paint = copy.deepcopy(message)
            else:
                if self.paint is None or self.paint["revision"] != message["base_revision"]:
                    raise AssertionError("Vim emitted a delta without its exact baseline")
                updates = {row["row"]: row["runs"] for row in message["row_updates"]}
                if len(updates) != len(message["row_updates"]):
                    raise AssertionError("Duplicate paint row")
                runs = [run for run in self.paint["runs"] if run["row"] not in updates]
                for row, replacement in updates.items():
                    if any(run["row"] != row for run in replacement):
                        raise AssertionError("Paint escaped its replacement row")
                    runs.extend(copy.deepcopy(replacement))
                self.paint = {key: copy.deepcopy(value) for key, value in message.items()
                              if key not in ("base_revision", "row_updates")}
                self.paint["event"] = "render"
                self.paint["runs"] = sorted(runs, key=lambda run: run["row"])

    def painted(self, predicate, timeout=6):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.paint is not None and predicate(self.paint):
                return copy.deepcopy(self.paint)
            self.pump(0.02)
        raise AssertionError(f"No expected Vim paint: {self.paint}")


@unittest.skipUnless(VIM, "Install Vim 9 for real native paint transport tests")
class RenderDeltaTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="jterm-vim9-delta-")
        self.root = Path(self.directory.name)
        self.editor = DeltaEditor(self.root)
        self.editor.file.write_text("".join(
            f"row {line:06d} original buffer text with colors and 中文\n"
            for line in range(1, 1001)), encoding="utf-8")
        self.editor.ex("edit!")
        self.editor.ex("set nonumber norelativenumber nocursorline")
        self.editor.ex("syntax match GuiOriginal /original/")
        self.editor.ex("highlight GuiOriginal guifg=#12ab34 ctermfg=1 gui=italic")
        self.first = self.editor.gui_frame()
        self.assertTrue(self.first["supported"], self.first.get("fallback_reason"))

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def assert_matches_keyframe(self):
        reconstructed = copy.deepcopy(self.editor.paint)
        keyframe = self.editor.gui_frame()
        self.assertEqual(canonical(reconstructed), canonical(keyframe))
        return keyframe

    def test_cursor_only_and_unicode_edit_transmit_incremental_paint(self):
        e = self.editor
        start = self.first["revision"]
        e.keys("l")
        delta = e.wait(lambda message: message.get("event") == "render_delta"
                       and message["revision"] > start)
        self.assertEqual(delta["row_updates"], [])
        self.assertNotEqual(delta["cursor"], self.first["cursor"])
        self.assertLess(len(json.dumps(delta)), len(json.dumps(self.first)) // 2)
        self.assert_matches_keyframe()
        start = e.paint["revision"]
        e.keys("i界e\u0301😀")
        edited = e.painted(lambda frame: frame["revision"] > start and frame["mode"] == "i"
                           and any(run["text"] == "界" for run in frame["runs"]))
        deltas = [event for event in e.paint_events if event["event"] == "render_delta"
                  and event["revision"] > start]
        self.assertTrue(deltas, "A one-line insertion must not resend this whole viewport")
        self.assertTrue(all(len(event["row_updates"]) <= 1 for event in deltas))
        self.assertTrue(any(run["text"] == "e\u0301" for run in edited["runs"]))
        self.assert_matches_keyframe()
        e.request("save")
        self.assertTrue(e.file.read_text(encoding="utf-8").startswith("r界e\u0301😀ow"))

    def test_empty_replacement_clears_old_glyphs_and_preserves_other_rows(self):
        e = self.editor
        start = self.first["revision"]
        e.keys("0D")
        delta = e.wait(lambda message: message.get("event") == "render_delta"
                       and message["revision"] > start
                       and any(not row["runs"] for row in message["row_updates"]))
        self.assertEqual(delta["row_updates"], [{"row": 0, "runs": []}])
        self.assertFalse(any(run["row"] == 0 for run in e.paint["runs"]))
        self.assert_matches_keyframe()
        e.request("save")
        self.assertTrue(e.file.read_text(encoding="utf-8").startswith("\nrow 000002"))

    def test_split_row_delta_keeps_peer_paint_and_keyframes_layout_changes(self):
        e = self.editor
        e.request("split", vertical=True)
        split = e.painted(lambda frame: frame.get("supported") and len(frame.get("windows", [])) == 2)
        self.assertEqual(e.paint_events[-1]["event"], "render")
        active = split["window"]
        e.request("open", path=str(e.other))
        e.state_where(lambda state: state["file"] == str(e.other))
        e.gui_frame()
        start = e.paint["revision"]
        e.keys("iNEW ")
        e.painted(lambda frame: frame["revision"] > start and frame["mode"] == "i"
                  and any("NEW " in run["text"] for run in frame["runs"]))
        delta = next(event for event in reversed(e.paint_events)
                     if event["event"] == "render_delta" and event["revision"] > start)
        changed = next(row for row in delta["row_updates"] if row["row"] == 0)
        self.assertEqual(len({run["window"] for run in changed["runs"]}), 2,
                         "Replacing a split screen row must retain its sibling")
        self.assertTrue(any(run["window"] != active and "row 000001" in run["text"]
                            for run in changed["runs"]))
        self.assert_matches_keyframe()
        e.request("save")
        self.assertTrue(e.other.read_text(encoding="utf-8").startswith("NEW other"))
        e.keys(b"\x1b")
        e.state_where(lambda state: state["mode"] == "n")
        e.request("split", vertical=False)
        e.painted(lambda frame: frame.get("supported") and len(frame.get("windows", [])) == 3)
        self.assertEqual(e.paint_events[-1]["event"], "render")

    def test_resync_and_compatibility_return_complete_frames_without_editing(self):
        e = self.editor
        e.keys("vll")
        e.painted(lambda frame: frame.get("mode") == "v")
        before = copy.deepcopy(e.paint)
        e.request("render_resync")
        after = e.painted(lambda frame: frame["revision"] > before["revision"])
        self.assertEqual(e.paint_events[-1]["event"], "render")
        self.assertEqual(canonical(before), canonical(after))
        e.keys(b"\x1b")
        e.state_where(lambda state: state["mode"] == "n")
        e.ex("set list")
        e.painted(lambda frame: not frame.get("supported", False))
        self.assertEqual(e.paint_events[-1]["event"], "render")
        e.ex("set nolist")
        e.painted(lambda frame: frame.get("supported", False))
        self.assertEqual(e.paint_events[-1]["event"], "render")
        self.assertFalse(e.state["modified"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
