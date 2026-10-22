#!/usr/bin/env python3
"""Window Normal colors and immutable source frames against real Vim."""

import json
from pathlib import Path
import tempfile
import unittest

from test_bridge import VIM
from test_render_delta import DeltaEditor, canonical


class WindowColorEditor(DeltaEditor):
    def authenticate(self):
        self.send({"action": "auth", "token": self.token, "protocol": 1,
                   "render_protocol": 5})
        return self.wait(lambda message: message.get("event") == "state")


@unittest.skipUnless(VIM, "Install Vim 9 for window color tests")
class RenderWindowColorTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-wincolor-")
        self.root = Path(self.directory.name)
        self.editor = WindowColorEditor(self.root)
        e = self.editor
        e.ex("set nonumber norelativenumber nocursorline nocursorcolumn notermguicolors")
        for group, fg, bg, term in [
            ("Normal", "eeeeee", "20242c", 0),
            ("WindowRed", "NONE", "731122", 1),
            ("WindowBlue", "abcdef", "112273", 4),
            ("CursorLine", "NONE", "114433", 2),
            ("ColorColumn", "NONE", "663311", 3),
            ("Visual", "NONE", "336677", 5),
        ]:
            e.ex(f"highlight {group} guifg={'NONE' if fg == 'NONE' else '#' + fg} "
                 f"guibg=#{bg} ctermfg=15 ctermbg={term} gui=NONE cterm=NONE")
        e.ex("call setline(1,['plain token', '', 'plain token']) | syntax match WindowWord /token/")
        e.ex("highlight WindowWord guifg=#aa8811 guibg=NONE gui=italic ctermfg=3 ctermbg=NONE cterm=italic")

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def native(self):
        frame = self.editor.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        return frame

    def cell(self, frame, row, column, window=None):
        window = window or frame["window"]
        return next((r["style"] for r in reversed(frame["runs"])
                     if r["window"] == window and r["row"] == row
                     and r["column"] <= column < r["column"] + r["cells"]), frame["default_style"])

    def oracle(self):
        self.editor.ex("redraw! | call writefile([json_encode(map(range(1,4), "
                       "{_,r -> map(range(1,16), {_,c -> screenattr(r,c)})}))], 'screen.json')")
        return json.loads((self.root / "screen.json").read_text())

    def has_winhighlight(self):
        self.editor.ex("call writefile([string(exists('+winhighlight'))], 'feature')")
        return (self.root / "feature").read_text().strip() == "1"

    def test_window_background_covers_empty_eol_eof_and_margins(self):
        e = self.editor
        e.ex("set wincolor=WindowRed")
        frame = self.native()
        self.assertEqual(frame["default_style"]["background"], 0x20242c)
        view = frame["windows"][0]["view"]
        for row in range(view["rows"]):
            background = next(r for r in frame["runs"] if r["row"] == row)
            self.assertEqual((background["line"], background["text"], background["cells"]),
                             (0, "", view["columns"]))
            self.assertEqual(background["byte_start"], background["byte_end"])
            self.assertEqual(self.cell(frame, row, view["columns"] - 1)["background"], 0x731122)
        self.assertEqual(self.cell(frame, 0, 0)["foreground"], 0xeeeeee)
        e.ex("call setline(1,map(range(1,80), {_,n -> 'plain token'})) | normal! 20Gzt")
        frame = self.native()
        window = next(w for w in frame["windows"] if w["window"] == frame["window"])
        self.assertTrue(window["overscan"])
        for edge in window["overscan"]:
            first = edge["runs"][0]
            self.assertEqual((first["line"], first["text"], first["cells"]), (0, "", view["columns"]))
            self.assertEqual(first["style"]["background"], 0x731122)

    def test_split_normal_is_isolated_while_syntax_style_cache_is_shared_safely(self):
        e = self.editor
        e.ex("set wincolor=WindowRed")
        first = self.native()
        original = first["window"]
        e.request("split", vertical=True)
        e.ex("set wincolor=WindowBlue")
        frame = self.native()
        active = frame["window"]
        for window in frame["windows"]:
            x, y = window["view"]["column"], window["view"]["row"]
            expected = 0x112273 if window["window"] == active else 0x731122
            self.assertEqual(self.cell(frame, y, x, window["window"])["background"], expected)
            word = self.cell(frame, y, x + 7, window["window"])
            self.assertEqual((word["foreground"], word["background"], word["italic"]),
                             (0xaa8811, expected, True))
        e.ex("wincmd p")
        switched = self.native()
        self.assertEqual(switched["window"], original)
        self.assertEqual(switched["changedtick"], first["changedtick"])
        self.assertEqual({w["window"] for w in switched["windows"]}, {original, active})

    def test_window_attributes_replace_normal_but_fg_bg_references_remain_global(self):
        e = self.editor
        e.ex("highlight Normal gui=bold cterm=bold | set wincolor=WindowRed")
        frame = self.native()
        self.assertFalse(self.cell(frame, 0, 0)["bold"])
        first = self.oracle()
        e.ex("highlight WindowRed gui=bold cterm=bold")
        self.assertTrue(self.cell(self.native(), 0, 0)["bold"])
        self.assertNotEqual(first[0][0], self.oracle()[0][0])
        e.ex("highlight WindowWord guifg=fg guibg=bg gui=NONE ctermfg=15 ctermbg=0 cterm=NONE")
        frame = self.native()
        self.assertEqual(self.cell(frame, 0, 7)["foreground"], 0xeeeeee)
        self.assertEqual(self.cell(frame, 0, 7)["background"], 0x20242c)
        e.ex("highlight WindowRed guifg=NONE guibg=NONE gui=nocombine ctermfg=NONE ctermbg=NONE cterm=nocombine")
        frame = self.native()
        self.assertFalse(self.cell(frame, 0, 0)["bold"])
        self.assertEqual(self.cell(frame, 0, 0)["background"], 0x20242c)

    def test_line_column_and_visual_overlays_keep_their_priority(self):
        e = self.editor
        e.ex("set wincolor=WindowRed cursorline colorcolumn=3 | normal! gg0")
        frame = self.native()
        self.assertEqual(self.cell(frame, 0, 0)["background"], 0x114433)
        self.assertEqual(self.cell(frame, 0, 2)["background"], 0x663311)
        self.assertEqual(self.cell(frame, 1, 0)["background"], 0x731122)
        e.keys("vll")
        e.state_where(lambda s: s["mode"] == "v")
        frame = self.native()
        self.assertEqual(self.cell(frame, 0, 2)["background"], 0x336677)

    def test_gutters_inherit_window_background_and_keep_explicit_groups(self):
        e = self.editor
        e.ex("set number signcolumn=yes wincolor=WindowRed | "
             "highlight LineNr guifg=#eeeeee guibg=NONE gui=NONE ctermfg=15 ctermbg=NONE cterm=NONE | "
             "highlight CursorLineNr guifg=#eeeeee guibg=NONE gui=NONE ctermfg=15 ctermbg=NONE cterm=NONE | "
             "highlight SignColumn guifg=#eeeeee guibg=NONE gui=NONE ctermfg=15 ctermbg=NONE cterm=NONE | "
             "syntax match GutterReference /plain/ | highlight! link GutterReference LineNr")
        frame = self.native()
        offset = frame["windows"][0]["textoff"]
        self.assertGreater(offset, 0)
        attrs = self.oracle()[0]
        self.assertEqual(attrs[:offset], [attrs[offset]] * offset)
        for column in range(offset):
            self.assertEqual(self.cell(frame, 0, column)["background"], 0x731122)
        e.ex("highlight LineNr guibg=#334455 ctermbg=2 | highlight CursorLineNr guibg=#334455 ctermbg=2")
        frame = self.native()
        self.assertEqual(self.cell(frame, 0, offset - 1)["background"], 0x334455)
        self.assertEqual(self.cell(frame, 0, offset)["background"], 0x334455)
        self.assertEqual(self.cell(frame, 0, 0)["background"], 0x731122)
        e.ex("call setline(1,repeat('plain token ',30)) | normal! 140|ze")
        frame = self.native()
        self.assertGreater(frame["windows"][0]["leftcol"], 0)
        self.assertEqual(self.cell(frame, 0, frame["view"]["columns"] - 1)["background"], 0x731122)

    def test_one_split_color_change_uses_delta_without_recoloring_peer(self):
        e = self.editor
        e.ex("set wincolor=WindowRed")
        e.request("split", vertical=False)
        e.ex("nnoremap <F9> <Cmd>setlocal wincolor=WindowBlue<CR>")
        first = self.native()
        active = first["window"]
        e.keys(b"\x1b[20~")
        changed = e.painted(lambda f: f["revision"] > first["revision"]
                            and self.cell(f, f["view"]["row"], f["view"]["column"])["background"] == 0x112273)
        for window in changed["windows"]:
            color = self.cell(changed, window["view"]["row"], window["view"]["column"], window["window"])["background"]
            self.assertEqual(color, 0x112273 if window["window"] == active else 0x731122)
        self.assertTrue(any(event["event"] == "render_delta" and event["revision"] > first["revision"]
                            for event in e.paint_events))
        self.assertEqual(changed["changedtick"], first["changedtick"])
        self.assertEqual(canonical(changed), canonical(self.native()))

    def test_window_color_changes_invalidate_paint_without_edits(self):
        e = self.editor
        e.ex("set wincolor=WindowRed | nnoremap <F9> <Cmd>setlocal wincolor=WindowBlue<CR>")
        first = self.native()
        e.keys(b"\x1b[20~")
        changed = e.painted(lambda f: f["revision"] > first["revision"]
                            and self.cell(f, 0, 0)["background"] == 0x112273)
        self.assertEqual(changed["changedtick"], first["changedtick"])
        self.assertEqual(canonical(changed), canonical(self.native()))
        e.ex("highlight WindowBlue guibg=#224466")
        self.assertEqual(self.cell(self.native(), 0, 0)["background"], 0x224466)
        e.ex("set wincolor=")
        self.assertEqual(self.cell(self.native(), 0, 0)["background"], 0x20242c)

    def test_effective_normal_alias_and_unsupported_mapping_fallback(self):
        if not self.has_winhighlight():
            self.skipTest("This Vim has no winhighlight option")
        e = self.editor
        e.ex("set wincolor=WindowRed winhighlight=")
        self.assertEqual(self.cell(self.native(), 0, 0)["background"], 0x20242c)
        e.ex("set winhighlight=Normal:WindowBlue")
        self.assertEqual(self.cell(self.native(), 0, 0)["background"], 0x112273)
        e.ex("set winhighlight=Visual:WindowBlue")
        frame = e.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertIn("remapping", frame["fallback_reason"])


if __name__ == "__main__":
    unittest.main()
