#!/usr/bin/env python3
"""Quick Open locations use real Vim buffers, byte columns and jump history.

The native pickers resolve a query to the existing ``open`` or ``goto`` bridge
command. These tests exercise those commands in a real Vim PTY, including names
that must remain literal and unsaved buffers that cannot be reopened from disk.
"""

from pathlib import Path
import tempfile
import unittest

from test_bridge import Editor, VIM


@unittest.skipUnless(VIM, "Install Vim 9 to exercise Quick Open navigation")
class QuickNavigationTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-quick-navigation-")
        self.root = Path(self.directory.name)
        self.editor = Editor(self.root)

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def test_literal_colon_filename_and_utf8_location_share_vims_jumplist(self):
        e = self.editor
        target = self.root / 'odd:42 #% | call writefile(["bad"],"PWNED") |.txt'
        contents = "first\nA界é🙂Z tail\nlast\n"
        target.write_text(contents, encoding="utf-8")
        e.ex("clearjumps")
        e.request("open", path=str(target), line=2, column=7)
        state = e.snapshot()
        self.assertEqual((state["file"], state["line"], state["column"]),
                         (str(target), 2, 7))
        self.assertFalse((self.root / "PWNED").exists())
        self.assertEqual(target.read_text(encoding="utf-8"), contents)
        e.request("navigate_back")
        state = e.snapshot()
        self.assertEqual((state["file"], state["line"], state["column"]),
                         (str(e.file), 1, 1))
        e.keys(b"\x09")  # Physical Ctrl-I and GUI navigation share one history.
        e.state_where(lambda s: s["file"] == str(target) and s["column"] == 7)
        e.request("open", path=str(target), line=3, column=2)
        e.request("navigate_back")
        state = e.snapshot()
        self.assertEqual((state["line"], state["column"]), (2, 7))

    def test_location_only_preserves_named_and_unnamed_unsaved_buffer(self):
        e = self.editor
        for unnamed in (False, True):
            with self.subTest(unnamed=unnamed):
                if unnamed:
                    e.ex("enew!")
                e.ex("call setline(1, ['first unsaved line', 'A界é🙂Z tail', 'last'])")
                e.ex("normal! gg0")
                e.ex("clearjumps")
                before = e.snapshot()
                before_buffer = next(b["id"] for b in before["buffers"] if b["current"])
                before_tick = e.gui_frame()["changedtick"]
                e.request("goto", line=2, column=7)
                state = e.snapshot()
                self.assertEqual((state["file"], state["line"], state["column"]),
                                 ("" if unnamed else str(e.file), 2, 7))
                self.assertTrue(state["modified"])
                self.assertEqual(next(b["id"] for b in state["buffers"] if b["current"]),
                                 before_buffer)
                self.assertEqual(e.gui_frame()["changedtick"], before_tick)
                e.request("navigate_back")
                state = e.snapshot()
                self.assertEqual((state["line"], state["column"]), (1, 1))
                e.request("navigate_forward")
                self.assertEqual(e.snapshot()["column"], 7)
        self.assertEqual(e.file.read_text(encoding="utf-8"), "original line\nsecond line\n")

    def test_byte_columns_and_out_of_bounds_locations_are_clamped_by_vim(self):
        e = self.editor
        target = self.root / "source:42"
        target.write_text("first\nA界é🙂Z tail\nlast\n", encoding="utf-8")
        e.request("open", path=str(target), line=2, column=8)
        state = e.snapshot()
        self.assertEqual((state["line"], state["column"]), (2, 7))
        e.request("goto", line=2, column=3)
        self.assertEqual(e.snapshot()["column"], 2)
        e.request("goto", line=999, column=999)
        state = e.snapshot()
        self.assertEqual((state["line"], state["column"]), (3, 4))
        self.assertFalse(state["modified"])


if __name__ == "__main__":
    unittest.main()
