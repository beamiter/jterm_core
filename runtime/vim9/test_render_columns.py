#!/usr/bin/env python3
"""Window-local column guides against actual Vim screen attributes."""

import json
from pathlib import Path
import tempfile
import unittest

from test_bridge import Editor, VIM


@unittest.skipUnless(VIM, "Install Vim 9 for native column guide tests")
class RenderColumnTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-columns-")
        self.root = Path(self.directory.name)
        self.editor = Editor(self.root, authenticate=False)
        self.editor.send({"action": "auth", "token": self.editor.token,
                          "protocol": 1, "render_protocol": 5})
        self.editor.wait(lambda message: message.get("event") == "state")
        self.editor.ex("set nonumber norelativenumber nocursorline nocursorcolumn nowrap notermguicolors")
        for group, rgb, terminal in [
            ("Normal", "20242c", 0), ("ColorColumn", "aa1100", 1),
            ("CursorColumn", "11aa00", 2), ("CursorLine", "0011aa", 3),
            ("Search", "aa00aa", 4), ("Visual", "00aaaa", 5),
            ("ColumnProperty", "aaaa00", 6),
        ]:
            self.editor.ex(f"highlight {group} guifg=#eeeeee guibg=#{rgb} "
                           f"ctermfg=15 ctermbg={terminal} gui=NONE cterm=NONE")
        self.editor.ex("call setline(1, ['abcdefghij', 'abcdefghij', '', 'x'])")

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def native(self):
        frame = self.editor.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        return frame

    def background(self, frame, line, column, window=None):
        window = window or frame["window"]
        view = next(w for w in frame["windows"] if w["window"] == window)
        row = view["view"]["row"] + line - view["topline"]
        screen_column = view["view"]["column"] + view["textoff"] + column - 1 - view["leftcol"]
        return next((r["style"]["background"] for r in reversed(frame["runs"])
                     if r["window"] == window and r["row"] == row
                     and r["column"] <= screen_column < r["column"] + r["cells"]),
                    frame["default_style"]["background"])

    def screen(self):
        # <Cmd> preserves Visual mode; entering an Ex prompt would remove its
        # live highlight before the oracle could observe it.
        e = self.editor
        e.ex("nnoremap <F8> <Cmd>redraw!<Bar>call writefile([json_encode(map(range(1,4), "
             "{_,r -> map(range(1,12), {_,c -> screenattr(r,c)})}))], 'screen.json')<CR>")
        e.ex("xnoremap <F8> <Cmd>redraw!<Bar>call writefile([json_encode(map(range(1,4), "
             "{_,r -> map(range(1,12), {_,c -> screenattr(r,c)})}))], 'screen.json')<CR>")

    def oracle(self):
        path = self.root / "screen.json"
        path.unlink(missing_ok=True)
        self.editor.keys(b"\x1b[19~")
        for _ in range(100):
            self.editor.pump(0.02)
            if path.exists():
                try:
                    return json.loads(path.read_text())
                except json.JSONDecodeError:
                    # The file can become visible just before writefile()
                    # finishes filling it; wait for the complete oracle.
                    continue
        self.fail("Vim did not publish the screen oracle")

    def test_cursor_and_color_columns_cover_text_empty_lines_and_eol(self):
        e = self.editor
        self.screen()
        e.ex("set colorcolumn=3,8 cursorcolumn | normal! gg02l")
        frame = self.native()
        for line in (1, 2, 3, 4):
            self.assertEqual(self.background(frame, line, 8), 0xaa1100)
            self.assertEqual(self.background(frame, line, 3), 0xaa1100 if line == 1 else 0x11aa00)
        attrs = self.oracle()
        self.assertEqual(attrs[0][2], attrs[0][7])
        self.assertEqual(attrs[1][2], attrs[2][2])
        self.assertNotEqual(attrs[0][2], attrs[1][2])
        guides = [r for r in frame["runs"] if r["line"] == 0 and r["text"] == ""]
        self.assertTrue(guides)
        self.assertTrue(all(r["byte_start"] == r["byte_end"] == 0 for r in guides))
        self.assertTrue(all(r["row"] < 4 for r in guides), "No invented document rows after EOF")
        before_tick = frame["changedtick"]
        e.keys("l")
        frame = self.native()
        self.assertEqual(frame["changedtick"], before_tick)
        self.assertEqual(self.background(frame, 2, 3), 0xaa1100)
        self.assertEqual(self.background(frame, 2, 4), 0x11aa00)

    def test_relative_columns_follow_textwidth_and_horizontal_view(self):
        e = self.editor
        e.ex("set textwidth=5 colorcolumn=2,-1,+1,+1,500 | normal! gg0")
        frame = self.native()
        for column in (2, 4, 6):
            self.assertEqual(self.background(frame, 2, column), 0xaa1100)
        e.ex("set textwidth=0")
        frame = self.native()
        self.assertEqual(self.background(frame, 2, 2), 0xaa1100)
        for column in (4, 6):
            self.assertEqual(self.background(frame, 2, column), 0x20242c)
        e.ex("call setline(1, repeat('x', 250)) | set colorcolumn=140 | normal! 150|ze")
        frame = self.native()
        self.assertGreater(frame["windows"][0]["leftcol"], 0)
        self.assertEqual(self.background(frame, 1, 140), 0xaa1100)

    def test_matches_visual_rows_and_properties_use_vims_priority(self):
        e = self.editor
        self.screen()
        e.ex("set cursorline cursorcolumn colorcolumn=3,8 | normal! gg02l")
        frame = self.native()
        self.assertEqual(self.background(frame, 1, 3), 0xaa1100)
        self.assertEqual(self.background(frame, 1, 4), 0x0011aa)
        e.ex("call matchadd('Search', 'c')")
        frame = self.native()
        self.assertEqual(self.background(frame, 1, 3), 0xaa00aa)
        self.assertEqual(self.background(frame, 2, 3), 0xaa00aa)
        e.ex("call clearmatches() | let @/ = 'c' | set hlsearch")
        frame = self.native()
        self.assertEqual(self.background(frame, 1, 3), 0xaa00aa)
        self.assertEqual(self.background(frame, 2, 3), 0xaa00aa)
        e.ex("nohlsearch")
        e.ex("call clearmatches() | set nocursorline | call prop_type_add('column', {'highlight':'ColumnProperty', 'override':1}) | call prop_add(2,3,{'type':'column','length':1})")
        self.assertEqual(self.background(self.native(), 2, 3), 0x11aa00)
        e.ex("call prop_clear(1,4)")
        e.keys("vj")
        e.state_where(lambda s: s["mode"] == "v")
        frame = self.native()
        attrs = self.oracle()
        self.assertEqual(self.background(frame, 2, 8), 0x20242c)
        self.assertEqual(attrs[1][7], attrs[1][8])
        self.assertEqual(self.background(frame, 1, 8), 0x00aaaa)

    def test_wide_glyphs_and_expanded_tabs_keep_source_bytes(self):
        e = self.editor
        self.screen()
        e.ex("call setline(1, ['a界bc', \"a\\tbc\", '', 'x']) | set tabstop=4 colorcolumn=3 | normal! gg0")
        frame = self.native()
        attrs = self.oracle()
        # A guide through the continuation cell does not split a wide glyph.
        self.assertEqual(self.background(frame, 1, 2), 0x20242c)
        self.assertEqual(self.background(frame, 1, 3), 0x20242c)
        self.assertEqual(attrs[0][1], attrs[0][2])
        self.assertEqual(self.background(frame, 2, 3), 0xaa1100)
        self.assertNotEqual(attrs[1][1], attrs[1][2])
        tab = [r for r in frame["runs"] if r["line"] == 2 and r["byte_start"] == 1 and r["byte_end"] == 2]
        self.assertEqual(sum(r["cells"] for r in tab), 3)
        self.assertEqual("".join(r["text"] for r in tab), "   ")
        e.ex("set colorcolumn=2")
        frame = self.native()
        attrs = self.oracle()
        self.assertEqual(self.background(frame, 1, 2), 0xaa1100)
        self.assertEqual(self.background(frame, 1, 3), 0xaa1100)
        self.assertEqual(attrs[0][1], attrs[0][2])

    def test_visual_suppresses_text_columns_but_keeps_eol_guides(self):
        e = self.editor
        self.screen()
        for mode in ("v", "V"):
            e.ex("call setline(1, ['abc', 'x', '', 'abc']) | set colorcolumn=3,8 cursorcolumn | normal! gg02l")
            e.keys(mode + "2j")
            e.state_where(lambda s: s["mode"] == mode)
            frame = self.native()
            attrs = self.oracle()
            # The selected source is suppressed/overlaid, while Vim's separate
            # EOL fill path still paints guide columns after short/empty lines.
            for line in (2, 3):
                for column in (3, 8):
                    self.assertEqual(self.background(frame, line, column), 0xaa1100)
                    self.assertEqual(attrs[line - 1][column - 1], attrs[3][column - 1])
            self.assertEqual(self.background(frame, 1, 3), 0x00aaaa)
            e.keys(b"\x1b")
            e.state_where(lambda s: s["mode"] == "n")

    def test_cursorcolumn_uses_the_actual_wide_and_tab_cursor_cell(self):
        e = self.editor
        self.screen()
        for literal, column in [('"a界bc"', 2), ('"a\\tbc"', 4), ('"aébc"', 2)]:
            e.ex("call setline(1,[" + literal + ",'abcdefghij','','x']) | "
                 "set cursorcolumn colorcolumn= tabstop=4 | call cursor(1,2)")
            frame = self.native()
            attrs = self.oracle()
            self.assertEqual(self.background(frame, 2, column), 0x11aa00)
            self.assertNotEqual(attrs[1][column - 1], attrs[1][0])
            for neighbor in {column - 1, column + 1}:
                if neighbor > 0:
                    self.assertEqual(self.background(frame, 2, neighbor), 0x20242c)

    def test_offscreen_and_ignored_relative_columns_emit_no_decorations(self):
        e = self.editor
        for options in ["colorcolumn=500", "colorcolumn=+1 textwidth=0"]:
            e.ex("set nocursorcolumn " + options)
            frame = self.native()
            self.assertFalse(any(r["line"] == 0 and r["text"] == "" for r in frame["runs"]))

    def test_one_column_split_guide_cannot_masquerade_as_window_normal(self):
        e = self.editor
        self.screen()
        e.ex("enew! | call setline(1,['a','b','']) | setlocal colorcolumn=1 nocursorcolumn | vsplit | vertical resize 1")
        frame = self.native()
        window = next(w for w in frame["windows"] if w["window"] == frame["window"])
        self.assertEqual(window["view"]["columns"], 1)
        self.assertEqual(window["textoff"], 0)
        runs = [r for r in frame["runs"] if r["window"] == window["window"]]
        self.assertFalse(any(r["line"] == 0 and r["text"] == "" and r["cells"] == 1
                             for r in runs))
        guides = [r for r in runs if r["line"] == 0 and r["style"]["background"] == 0xaa1100]
        self.assertEqual(len(guides), 3)
        self.assertTrue(all(r["text"] == " " and r["byte_start"] == r["byte_end"] == 0
                            for r in guides))
        attrs = self.oracle()
        self.assertEqual(attrs[0][0], attrs[1][0])
        self.assertEqual(attrs[1][0], attrs[2][0])
        self.assertNotEqual(attrs[2][0], attrs[3][0], "Vim does not color non-document EOF rows")
        self.assertEqual(self.background(frame, 1, 1), 0xaa1100)
        self.assertEqual(self.background(frame, 4, 1), 0x20242c)

    def test_column_changes_publish_deltas_and_margin_decorations(self):
        e = self.editor
        e.ex("call setline(1, map(range(1,80), {_,n -> 'abcdefghij'})) | set colorcolumn=3 cursorcolumn | normal! 20Gzt0")
        frame = self.native()
        window = next(w for w in frame["windows"] if w["window"] == frame["window"])
        self.assertTrue(window["overscan"])
        for edge in window["overscan"]:
            self.assertTrue(any(r["line"] == 0 and r["text"] == "" and r["cells"] == 1
                                and r["byte_start"] == r["byte_end"] == 0
                                for r in edge["runs"]))
        e.keys("l")
        delta = e.wait(lambda message: message.get("event") == "render_delta"
                       and message["revision"] > frame["revision"])
        self.assertEqual(delta["changedtick"], frame["changedtick"])
        self.assertTrue(delta["row_updates"])
        e.ex("nnoremap <F9> <Cmd>setlocal colorcolumn=8<CR>")
        frame = self.native()
        e.keys(b"\x1b[20~")
        # Changing every row can be smaller as a full checkpoint; either
        # transport must carry the current window-local option immediately.
        delta = e.wait(lambda message: message.get("event") in ("render", "render_delta")
                       and message.get("supported")
                       and message["revision"] > frame["revision"])
        self.assertEqual(delta["changedtick"], frame["changedtick"])
        frame = self.native()
        self.assertEqual(self.background(frame, 21, 8), 0xaa1100)
        self.assertEqual(self.background(frame, 21, 3), 0x20242c)

    def test_split_guides_are_window_local_without_focus_or_edit_changes(self):
        e = self.editor
        e.ex("set colorcolumn=3 cursorcolumn | normal! gg04l")
        first = self.native()
        original = first["window"]
        e.request("split", vertical=True)
        e.ex("set colorcolumn=8 nocursorcolumn | normal! j0")
        frame = self.native()
        active = frame["window"]
        self.assertNotEqual(active, original)
        self.assertEqual(self.background(frame, 2, 3, original), 0xaa1100)
        self.assertEqual(self.background(frame, 2, 5, original), 0x11aa00)
        self.assertEqual(self.background(frame, 2, 8, active), 0xaa1100)
        self.assertEqual(self.background(frame, 2, 3, active), 0x20242c)
        self.assertEqual(frame["changedtick"], first["changedtick"])
        self.assertEqual(self.native()["window"], active)


if __name__ == "__main__":
    unittest.main()
