#!/usr/bin/env python3
"""Wrapped continuation rows painted natively (render protocol 9) against actual Vim."""

import json
from pathlib import Path
import tempfile
import unittest

from test_bridge import Editor, VIM
from test_render_folds import visible_grid


@unittest.skipUnless(VIM, "Install Vim 9 for native wrap rendering tests")
class RenderWrapTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-wrap-")
        self.root = Path(self.directory.name)
        self.editor = self.start(9)

    def start(self, protocol):
        e = Editor(self.root, authenticate=False, vimrc_extra="set wrap smoothscroll display=lastline\n")
        e.send({"action": "auth", "token": e.token, "protocol": 1, "render_protocol": protocol})
        e.wait(lambda message: message.get("event") == "state")
        e.ex("set notermguicolors nocursorline")
        for group, rgb, terminal in [
            ("Normal", "20242c", 0), ("NonText", "445566", 1), ("CursorLine", "0011aa", 2),
            ("Visual", "00aaaa", 3), ("LineNr", "556677", 4), ("CursorLineNr", "778899", 5),
            ("SignColumn", "123456", 6), ("Search", "aa00aa", 7),
        ]:
            e.ex(f"highlight {group} guifg=#eeeeee guibg=#{rgb} ctermfg=15 ctermbg={terminal} gui=NONE cterm=NONE")
        command = ("<Cmd>redraw!<Bar>call writefile([json_encode(map(range(1,&lines), "
                   "{_,r -> map(range(1,&columns), {_,c -> [screenstring(r,c), screenattr(r,c)]})}))], "
                   "'screen.json')<CR>")
        e.ex("nnoremap <F8> " + command)
        e.ex("xnoremap <F8> " + command)
        # A narrow split exercises continuation rows with a short text width.
        e.ex("vsplit | vertical resize 33")
        return e

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def native(self):
        frame = self.editor.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        return frame

    def oracle(self):
        e = self.editor
        path = self.root / "screen.json"
        path.unlink(missing_ok=True)
        e.keys(b"\x1b[19~")
        for _ in range(200):
            e.pump(0.02)
            if path.exists():
                try:
                    return json.loads(path.read_text())
                except json.JSONDecodeError:
                    continue
        self.fail("Vim did not publish the screen oracle")

    def window(self, frame, window=None):
        window = window or frame["window"]
        return next(w for w in frame["windows"] if w["window"] == window)

    def assert_screen(self, frame=None, window=None):
        frame = frame or self.native()
        screen = self.oracle()
        window = window or frame["window"]
        view = self.window(frame, window)["view"]
        grid = visible_grid(frame, window)
        for row in range(view["row"], view["row"] + view["rows"]):
            expected = "".join(screen[row][c][0] or " " for c in range(view["column"], view["column"] + view["columns"]))
            actual = "".join(grid.get((row, c), " ") for c in range(view["column"], view["column"] + view["columns"]))
            self.assertEqual(actual.rstrip(), expected.rstrip(), f"screen row {row}")
        return frame, screen

    def attr_groups(self, frame, screen, row, columns):
        """Native backgrounds and Vim attribute IDs for the same cells."""
        native = []
        for column in columns:
            native.append(next((r["style"].get("background") for r in reversed(frame["runs"])
                                if r["row"] == row and r["column"] <= column < r["column"] + r["cells"]
                                and r["style"].get("background") is not None),
                               frame["default_style"]["background"]))
        return native, [screen[row][c][1] for c in columns]

    def assert_same_partition(self, native, vim, label):
        """Equal Vim attributes must paint equal backgrounds and vice versa."""
        for i in range(len(native)):
            for j in range(len(native)):
                self.assertEqual(native[i] == native[j], vim[i] == vim[j], f"{label}: cells {i}/{j}")

    def frame_where(self, predicate):
        """Typed keys reach Vim through the PTY; wait for their viewport."""
        for _ in range(50):
            frame = self.native()
            if predicate(frame):
                return frame
            self.editor.pump(0.05)
        self.fail("Vim did not reach the expected viewport")

    def lines(self, *lines):
        self.editor.ex("call setline(1, " + json.dumps(list(lines), ensure_ascii=False) + ") | normal! gg")

    def test_plain_wrap_gutters_and_row_map_match_vim(self):
        e = self.editor
        e.ex("set number signcolumn=yes cursorline")
        e.ex("call sign_define('mark', {'text': '>>'}) | call sign_place(1, '', 'mark', bufnr(), {'lnum': 2})")
        self.lines("short", "abcdefghij" * 7, "third", "\tindented words that go on past the edge of this window", "tail")
        e.ex("normal! 2G")
        frame, screen = self.assert_screen()
        window = self.window(frame)
        self.assertEqual(window["display_lines"][:6], [1, 2, 2, 2, 3, 4])
        self.assertEqual(window["wrap_parts"][:6], [0, 0, 1, 2, 0, 0])
        self.assertEqual(window["wrap_rows"][1], 3)
        self.assertEqual(window["skipcol"], 0)
        self.assertFalse(window["native_pointer"])
        self.assertTrue(window["native_scroll"])
        row = window["view"]["row"]
        # CursorLineNr and CursorLine cover every row of the wrapped cursor line.
        native, vim = self.attr_groups(frame, screen, row + 2, range(0, 12))
        self.assert_same_partition(native, vim, "cursor continuation row")
        native, vim = self.attr_groups(frame, screen, row + 5, range(0, 12))
        self.assert_same_partition(native, vim, "ordinary first row")
        runs = [r for r in frame["runs"] if r["line"] == 2]
        self.assertEqual(sorted({r["row"] - row for r in runs if r["text"].strip()}), [1, 2, 3])
        self.assertEqual(max(r["byte_end"] for r in runs), 70)

    def test_inactive_split_and_closed_folds_between_wrapped_lines(self):
        e = self.editor
        e.ex("set number relativenumber cursorline")
        self.lines("first", "abcdefghij" * 9, *[f"fold {n}" for n in range(3, 9)], "abcdefghij" * 5, "tail")
        e.ex("3,8fold | normal! gg")
        frame, screen = self.assert_screen()
        window = self.window(frame)
        self.assertIn(8, window["fold_ends"])
        self.assertEqual(window["display_lines"][:6], [1, 2, 2, 2, 2, 3])
        self.assertEqual(window["wrap_parts"][5], 0)
        other = next(w for w in frame["windows"] if w["window"] != frame["window"])
        self.assertNotEqual(other["view"]["columns"], window["view"]["columns"])
        grid = visible_grid(frame, other["window"])
        view = other["view"]
        for row in range(view["row"], view["row"] + view["rows"]):
            expected = "".join(screen[row][c][0] or " " for c in range(view["column"], view["column"] + view["columns"]))
            actual = "".join(grid.get((row, c), " ") for c in range(view["column"], view["column"] + view["columns"]))
            self.assertEqual(actual.rstrip(), expected.rstrip(), f"inactive split row {row}")
        e.keys("gg\x05")
        frame = self.frame_where(lambda f: self.window(f)["topline"] == 2)
        window = self.window(frame)
        self.assertEqual(window["display_lines"][:5], [2, 2, 2, 2, 3])
        above = [edge for edge in window["overscan"] if edge["row"] < 0]
        self.assertEqual((above[-1]["line"], above[-1]["part"], above[-1]["parts"]), (1, 0, 1))
        self.assert_screen(frame)

    def test_wide_glyph_fillers_tabs_and_unicode_match_vim(self):
        self.lines("wide: " + "界" * 20, "x" * 32 + "\tafter" + "y" * 30, "é" * 40)
        frame, _ = self.assert_screen()
        own = [r for r in frame["runs"] if r["window"] == frame["window"]]
        fillers = [r for r in own if r["text"] == ">" and r["line"] == 0]
        self.assertTrue(fillers, "a wide glyph that does not fit leaves Vim's '>' cell")
        tab = [r for r in own if r["line"] == 2 and r["byte_start"] == 32 and r["byte_end"] == 33]
        self.assertEqual(len(tab), 2, "a tab across a row boundary paints on both rows")
        self.assertEqual(sum(r["cells"] for r in tab), 8)
        self.editor.ex("set number")
        self.assert_screen()

    def test_linebreak_breakindent_and_showbreak_match_vim(self):
        e = self.editor
        self.lines("\tindented words that go on and on past the edge of this window",
                   "plain " + "abcdefghij" * 6, "wide: " + "界" * 20)
        for options in ["linebreak", "linebreak breakindent", "linebreak breakindent showbreak=>>\\ ",
                        "linebreak breakindent showbreak=>>\\  breakindentopt=sbr",
                        "nolinebreak nobreakindent breakindentopt= showbreak=+",
                        "number linebreak breakindent showbreak=↪\\ "]:
            e.ex("set " + options)
            frame, _ = self.assert_screen()
            self.assertTrue(frame["windows"][0]["wrap_rows"], options)

    def test_smoothscroll_first_row_matches_vim(self):
        e = self.editor
        self.lines("line 1", "abcdefghij" * 8 + "12345", *[f"line {n}" for n in range(3, 40)])
        for options in ["nonumber signcolumn=no", "number", "number relativenumber",
                        "norelativenumber nonumber relativenumber", "number relativenumber signcolumn=yes",
                        "norelativenumber signcolumn=no showbreak=+++", "showbreak= cursorline"]:
            e.ex("set " + options)
            e.keys("gg\x05\x05")
            frame, _ = self.assert_screen(self.frame_where(lambda f: self.window(f)["skipcol"] > 0))
            window = self.window(frame)
            self.assertGreater(window["skipcol"], 0, options)
            self.assertEqual(window["display_lines"][0], 2)
            self.assertGreater(window["wrap_parts"][0], 0)
            above = [edge for edge in window["overscan"] if edge["row"] < 0]
            self.assertEqual((above[-1]["line"], above[-1]["part"]), (2, window["wrap_parts"][0] - 1),
                             "the parts hidden by smoothscroll are the rows just above")
        e.ex("set showbreak= nocursorline | call setline(2, repeat(nr2char(0x754c), 40))")
        e.keys("gg\x05\x05")
        self.assert_screen(self.frame_where(lambda f: self.window(f)["skipcol"] > 0))

    def test_unfinished_last_line_follows_display(self):
        e = self.editor
        e.ex("set number | 8split")
        self.lines("line 1", "b" * 60, "line 3", "line 4", "e" * 60, "line 6")
        for display, expect in [("lastline", "@@@"), ("truncate", "@@@"), ("", "@")]:
            e.ex(f"set display={display}")
            frame, _ = self.assert_screen()
            window = self.window(frame)
            texts = [r["text"] for r in frame["runs"] if r["row"] >= window["view"]["row"]]
            self.assertTrue(any(t.strip() == expect for t in texts), display)
            below = [edge for edge in window["overscan"] if edge["row"] >= window["view"]["rows"]]
            if display == "lastline":
                self.assertEqual(below[0]["line"], window["display_lines"][-1])
            else:
                self.assertEqual(below, [], "rows replaced by @ lines are not contiguous text")

    def test_scroll_view_moves_wrapped_rows_and_acknowledges_them(self):
        e = self.editor
        self.lines("line 1", "abcdefghij" * 10, *[f"line {n}" for n in range(3, 60)])
        frame = self.native()
        window = self.window(frame)
        e.request("scroll_view", window=window["window"], buffer=window["buffer"],
                  changedtick=window["changedtick"], topline=1, leftcol=0, skipcol=0, lines=2)
        window = self.window(self.native())
        self.assertEqual((window["topline"], window["scroll_rows"]), (2, 2))
        self.assertGreater(window["skipcol"], 0, "smoothscroll stopped inside the wrapped line")
        e.request("scroll_view", window=window["window"], buffer=window["buffer"],
                  changedtick=window["changedtick"], topline=2, leftcol=0, skipcol=window["skipcol"], lines=4)
        window = self.window(self.native())
        self.assertEqual(window["scroll_rows"], 4)
        e.request("scroll_view", window=window["window"], buffer=window["buffer"],
                  changedtick=window["changedtick"], topline=window["topline"], leftcol=0,
                  skipcol=window["skipcol"] + 1, lines=1, ok=False)
        self.assert_screen()

    def test_visual_rows_search_and_eob_fillchars_match_vim(self):
        e = self.editor
        e.ex("set fillchars=eob:\\  hlsearch | let @/ = 'cd'")
        self.lines("abcdefghij" * 5, "x" * 31, "tail")
        self.assert_screen()
        e.keys("Vj")
        e.state_where(lambda s: s["mode"] == "V")
        frame, screen = self.assert_screen()
        view = self.window(frame)["view"]
        for row in range(view["row"], view["row"] + 3):
            # Vim leaves the cursor cell itself out of the Visual highlight.
            columns = [c for c in range(view["column"], view["column"] + view["columns"])
                       if (row, c) != (frame["cursor"]["row"], frame["cursor"]["column"])]
            native, vim = self.attr_groups(frame, screen, row, columns)
            self.assert_same_partition(native, vim, f"Visual row {row}")
        e.keys(b"\x1b")
        e.state_where(lambda s: s["mode"] == "n")
        e.keys(b"\x16j")
        e.state_where(lambda s: s["mode"] == "\x16")
        frame = e.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertIn("wrapped Vim lines", frame["fallback_reason"])

    def test_cursor_line_number_needs_cursorline_like_vim(self):
        e = self.editor
        e.ex("set nowrap number nocursorline | normal! 2G")
        self.lines("one", "two", "three")
        e.ex("normal! 2G")
        frame, screen = self.assert_screen()
        row = self.window(frame)["view"]["row"]
        native, vim = self.attr_groups(frame, screen, row + 1, range(0, 3))
        other, vim_other = self.attr_groups(frame, screen, row, range(0, 3))
        self.assertEqual(vim[0] == vim_other[0], native[0] == other[0],
                         "without cursorline the cursor row number uses LineNr")

    def test_older_protocols_keep_the_compatibility_view_for_wrapped_rows(self):
        self.editor.close()
        self.editor = self.start(8)
        self.lines("abcdefghij" * 10, "short")
        frame = self.editor.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertIn("Wrapped", frame["fallback_reason"])


if __name__ == "__main__":
    unittest.main()
