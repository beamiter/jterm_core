#!/usr/bin/env python3
"""Real Vim viewport commands: split focus, input modes, bounds and no edits."""

import tempfile
from pathlib import Path
import unittest

from test_bridge import Editor, VIM


@unittest.skipUnless(VIM, "Install Vim 9 to exercise native scrolling")
class NativeScrollTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-scroll-")
        self.editor = Editor(Path(self.directory.name))
        self.editor.ex("call setline(1, map(range(1, 240), {_, n -> printf('%03d 界 é', n)}))")
        self.editor.ex("set nowrap scrolloff=0")
        self.editor.ex("normal! gg0")

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def frame(self):
        frame = self.editor.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        return frame

    def test_scrolls_hovered_split_without_focus_edit_or_peer_motion(self):
        e = self.editor
        e.ex("vsplit")
        before = self.frame()
        active = before["window"]
        target = next(w for w in before["windows"] if w["window"] != active)
        peer = next(w for w in before["windows"] if w["window"] == active)
        e.request("scroll", window=target["window"], lines=4)
        after = self.frame()
        self.assertEqual(after["window"], active)
        moved = next(w for w in after["windows"] if w["window"] == target["window"])
        steady = next(w for w in after["windows"] if w["window"] == active)
        self.assertEqual(moved["topline"], target["topline"] + 4)
        self.assertEqual(steady["topline"], peer["topline"])
        self.assertEqual(moved["changedtick"], target["changedtick"])
        self.assertEqual(after["cursor"], before["cursor"])
        e.request("scroll", window=target["window"], lines=-3)
        restored = next(w for w in self.frame()["windows"] if w["window"] == target["window"])
        self.assertEqual(restored["topline"], target["topline"] + 1)

    def test_rejects_closed_windows_invalid_counts_and_command_lines(self):
        e = self.editor
        current = self.frame()["window"]
        for count in [0, 121, -121, -9223372036854775808, "2", True]:
            e.request("scroll", window=current, lines=count, ok=False)
        e.request("scroll", window=999999, lines=1, ok=False)
        e.keys(":echo ")
        e.state_where(lambda s: s["mode"].startswith("c"))
        e.request("scroll", window=current, lines=2, ok=False)
        e.keys(b"\x1b")
        e.state_where(lambda s: s["mode"] == "n")

    def test_insert_mode_and_unicode_input_survive_semantic_scroll(self):
        e = self.editor
        e.ex("normal! 12G0")
        e.keys("i")
        e.state_where(lambda s: s["mode"] == "i")
        before = self.frame()
        e.request("scroll", window=before["window"], lines=2)
        after = self.frame()
        self.assertEqual(after["mode"], "i")
        self.assertEqual(after["changedtick"], before["changedtick"])
        e.keys("丝滑")
        e.keys(b"\x1b")
        e.state_where(lambda s: s["mode"] == "n")
        e.request("save")
        saved = e.file.read_text(encoding="utf-8")
        self.assertIn("丝滑012 界 é", saved)

    def test_top_and_bottom_clamp_without_changing_document(self):
        e = self.editor
        before = self.frame()
        e.request("scroll", window=before["window"], lines=-120)
        self.assertEqual(self.frame()["windows"][0]["topline"], 1)
        for _ in range(4):
            e.request("scroll", window=before["window"], lines=120)
        end = self.frame()
        self.assertLessEqual(end["windows"][0]["topline"], 240)
        self.assertEqual(end["changedtick"], before["changedtick"])


if __name__ == "__main__":
    unittest.main()
