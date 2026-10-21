#!/usr/bin/env python3
"""Native cursor appearance from real Vim options, modes and GUI highlights."""
from pathlib import Path
import tempfile
import unittest

from test_bridge import VIM
from test_render_delta import DeltaEditor, canonical


@unittest.skipUnless(VIM, "Install Vim 9 for native cursor tests")
class RenderCursorTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-cursor-")
        self.editor = DeltaEditor(Path(self.directory.name))
        self.editor.ex("set nonumber norelativenumber nocursorline notermguicolors")
        self.editor.ex("highlight NativeCursor guifg=#102030 guibg=#e1a273 ctermfg=1 ctermbg=2 gui=NONE")
        self.editor.ex("highlight NativeInsert guifg=#203040 guibg=#73a2e1 gui=NONE")
        self.editor.ex("set guicursor=n-v:block-NativeCursor,i:ver30-NativeInsert,r:hor40-NativeCursor,a:blinkwait350-blinkon120-blinkoff80")
        self.editor.gui_frame()

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def appearance(self):
        frame = self.editor.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        return frame["cursor"]["appearance"]

    def test_native_colors_and_normal_insert_replace_shapes(self):
        e = self.editor
        normal = self.appearance()
        self.assertEqual(normal["shape"], "block")
        self.assertEqual(normal["style"]["background"], 0xe1a273)
        self.assertEqual(normal["style"]["foreground"], 0x102030)
        self.assertEqual([normal[k] for k in ["blinkwait", "blinkon", "blinkoff"]], [350, 120, 80])
        e.keys("i")
        insert = e.painted(lambda f: f["mode"] == "i")["cursor"]["appearance"]
        self.assertEqual((insert["shape"], insert["percentage"]), ("vertical", 30))
        self.assertEqual(insert["style"]["background"], 0x73a2e1)
        e.keys("\x1bR")
        replace = e.painted(lambda f: f["mode"].startswith("R"))["cursor"]["appearance"]
        self.assertEqual((replace["shape"], replace["percentage"]), ("horizontal", 40))

    def test_ordered_all_mode_overlays_and_specific_mode_reset(self):
        e = self.editor
        e.ex("set guicursor=a:blinkon0,n:ver20-NativeCursor")
        a = self.appearance()
        self.assertEqual((a["blinkon"], a["blinkwait"]), (400, 700))
        e.ex("set guicursor=n:ver20-NativeCursor,a:blinkon0")
        self.assertEqual(self.appearance()["blinkon"], 0)

    def test_exclusive_visual_inherits_v_unless_ve_is_explicit(self):
        e = self.editor
        e.ex("set selection=exclusive guicursor=v:hor45-NativeCursor,ve:ver15-NativeInsert")
        e.keys("v")
        a = e.painted(lambda f: f["mode"] == "v")["cursor"]["appearance"]
        self.assertEqual((a["shape"], a["percentage"]), ("vertical", 15))
        e.keys("\x1b")
        e.ex("set guicursor=v:hor45-NativeCursor")
        e.keys("v")
        a = e.painted(lambda f: f["mode"] == "v" and f["cursor"]["appearance"]["shape"] == "horizontal")["cursor"]["appearance"]
        self.assertEqual(a["percentage"], 45)

    def test_language_mapping_uses_second_linked_highlight(self):
        e = self.editor
        e.ex("highlight! link NativeLinked NativeInsert")
        e.ex("set guicursor=n-i:block-NativeCursor/NativeLinked iminsert=1")
        self.assertEqual(self.appearance()["style"]["background"], 0xe1a273)
        e.keys("i")
        e.painted(lambda f: f["mode"] == "i")
        self.assertEqual(self.appearance()["style"]["background"], 0x73a2e1)
        e.keys("\x1b")
        e.ex("set iminsert=0")
        self.assertEqual(self.appearance()["style"]["background"], 0xe1a273)

    def test_cursor_theme_change_is_a_delta_without_repainting_text(self):
        e = self.editor
        first = e.gui_frame()
        e.ex("highlight NativeCursor guibg=#ae12cd")
        updated = e.painted(lambda f: f["revision"] > first["revision"] and f["mode"] == "n"
                            and f["cursor"]["appearance"]["style"]["background"] == 0xae12cd)
        self.assertEqual(first["runs"], updated["runs"])
        self.assertTrue(any(m["event"] == "render_delta" and not m["row_updates"]
                            and m["revision"] > first["revision"] for m in e.paint_events))
        self.assertEqual(canonical(updated), canonical(e.gui_frame()))

    def test_wide_combining_and_empty_line_keep_source_geometry(self):
        e = self.editor
        e.ex("call setline(1, ['界é', '']) | normal! gg0")
        frame = e.gui_frame()
        self.assertEqual(frame["cursor"]["width"], 2)
        self.assertEqual(frame["cursor"]["byte_column"], 0)
        e.keys("l")
        frame = e.painted(lambda f: f["cursor"]["byte_column"] == 3)
        self.assertEqual(frame["cursor"]["width"], 1)
        e.keys("j")
        frame = e.painted(lambda f: f["cursor"]["line"] == 2)
        self.assertTrue(frame["cursor"]["visible"])
        self.assertEqual(frame["cursor"]["byte_column"], 0)


if __name__ == "__main__":
    unittest.main()
