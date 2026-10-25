#!/usr/bin/env python3
"""Closed folds painted natively (render protocol 8) against actual Vim."""

import json
import os
from pathlib import Path
import tempfile
import unittest

from test_bridge import Editor, VIM

VIMRC_ROOT = Path(os.environ.get(
    "VIM9_TEST_VIMRC", Path(__file__).resolve().parents[3] / "vimrc")).resolve()


def visible_grid(frame, window=None):
    """Rebuild the painted cells of one split from its native runs."""
    grid = {}
    for run in frame["runs"]:
        if window is not None and run["window"] != window:
            continue
        text = run["text"]
        if not text:
            continue
        column = run["column"]
        if len(text) == run["cells"]:
            for offset, character in enumerate(text):
                grid[(run["row"], column + offset)] = character
        else:
            grid[(run["row"], column)] = text
    return grid


@unittest.skipUnless(VIM, "Install Vim 9 for native fold rendering tests")
class RenderFoldTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-folds-")
        self.root = Path(self.directory.name)
        self.editor = Editor(self.root, authenticate=False)
        self.connect(8)
        e = self.editor
        e.ex("set notermguicolors nocursorline nocursorcolumn nowrap foldmethod=manual")
        for group, rgb, terminal in [
            ("Normal", "20242c", 0), ("Folded", "334455", 1), ("CursorLine", "0011aa", 2),
            ("Visual", "00aaaa", 3), ("ColorColumn", "aa1100", 4), ("CursorColumn", "11aa00", 5),
            ("LineNr", "556677", 6), ("CursorLineNr", "778899", 7),
        ]:
            e.ex(f"highlight {group} guifg=#eeeeee guibg=#{rgb} ctermfg=15 ctermbg={terminal} gui=NONE cterm=NONE")
        e.ex("call setline(1, map(range(1, 400), 'printf(\"%d\\tline %d\", v:val, v:val)'))")
        self.install_oracle()

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def connect(self, protocol):
        e = self.editor
        e.send({"action": "auth", "token": e.token, "protocol": 1, "render_protocol": protocol})
        e.wait(lambda message: message.get("event") == "state")

    def native(self):
        frame = self.editor.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        return frame

    def install_oracle(self):
        # <Cmd> keeps Visual mode alive while Vim publishes its own screen.
        command = ("<Cmd>redraw!<Bar>call writefile([json_encode(map(range(1,&lines), "
                   "{_,r -> map(range(1,&columns), {_,c -> [screenstring(r,c), screenattr(r,c)]})}))], "
                   "'screen.json')<CR>")
        self.editor.ex("nnoremap <F8> " + command)
        self.editor.ex("xnoremap <F8> " + command)

    def oracle(self):
        """Vim's own screen text and attribute IDs, after a full redraw."""
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

    def assert_matches_screen(self, frame, screen, window=None):
        window = window or frame["window"]
        view = next(w for w in frame["windows"] if w["window"] == window)["view"]
        grid = visible_grid(frame, window)
        for row in range(view["row"], view["row"] + view["rows"]):
            expected, actual = [], []
            column = view["column"]
            while column < view["column"] + view["columns"]:
                text = screen[row][column][0]
                expected.append(text or " ")
                actual.append(grid.get((row, column), " "))
                column += 1
            self.assertEqual("".join(actual).rstrip(), "".join(expected).rstrip(), f"screen row {row}")

    def window(self, frame, window=None):
        window = window or frame["window"]
        return next(w for w in frame["windows"] if w["window"] == window)

    def background(self, frame, row, column):
        return next((r["style"]["background"] for r in reversed(frame["runs"])
                     if r["row"] == row and r["column"] <= column < r["column"] + r["cells"]),
                    frame["default_style"]["background"])

    def test_fold_rows_match_vims_screen_and_publish_a_row_map(self):
        e = self.editor
        e.ex("set number relativenumber numberwidth=5 signcolumn=yes")
        e.ex("22,30fold | 40,45fold | 10,12fold | normal! 25G")
        e.ex("normal! 15Gzt")
        frame = self.native()
        self.assert_matches_screen(frame, self.oracle())
        window = self.window(frame)
        self.assertEqual(window["topline"], 15)
        self.assertEqual(window["display_lines"][:9], [15, 16, 17, 18, 19, 20, 21, 22, 31])
        self.assertEqual(window["fold_ends"][7], 30)
        self.assertEqual(len(window["display_lines"]), window["view"]["rows"])
        self.assertEqual(len(window["line_lengths"]), len(window["display_lines"]))
        self.assertEqual(window["botline"], window["display_lines"][-1])
        self.assertFalse(window["native_pointer"], "Vim owns clicks while fold rows are visible")
        self.assertTrue(window["native_scroll"])
        fold = next(r for r in frame["runs"] if r["text"].startswith("+--"))
        self.assertEqual((fold["line"], fold["byte_start"], fold["byte_end"]), (0, 0, 0))
        self.assertNotIn(23, {r["line"] for r in frame["runs"]}, "Hidden fold lines are never painted")
        above = {edge["row"]: edge for edge in window["overscan"] if edge["row"] < 0}
        self.assertEqual((above[-3]["line"], above[-3]["fold_end"]), (10, 12))
        self.assertEqual(above[-2]["line"], 13)
        self.assertNotIn("fold_end", above[-2])
        self.assertTrue(any(r["text"].startswith("+--  3 lines") for r in above[-3]["runs"]))

    def test_numbers_signs_text_and_fill_follow_fold_line(self):
        e = self.editor
        e.ex("set number norelativenumber signcolumn=yes fillchars=fold:·")
        e.ex("call sign_define('mark', {'text': '>>'}) | call sign_place(1, '', 'mark', bufnr(), {'lnum': 22})")
        e.ex("set foldtext=v:folddashes.'X界'.v:foldstart | 22,30fold | normal! 20Gzt")
        frame = self.native()
        screen = self.oracle()
        self.assert_matches_screen(frame, screen)
        row = self.window(frame)["view"]["row"] + 2
        self.assertEqual(screen[row][0][0] + screen[row][1][0], "  ", "No sign on a closed fold")
        wide = next(r for r in frame["runs"] if r["text"] == "界")
        self.assertEqual(wide["cells"], 2)
        fill = [r for r in frame["runs"] if r["row"] == row and r["text"].startswith("·")]
        self.assertEqual(len(fill), 1)
        self.assertEqual(fill[0]["column"] + fill[0]["cells"], 120)
        self.assertEqual(len(fill[0]["text"]), fill[0]["cells"])
        e.ex("set nonumber signcolumn=no")
        self.assert_matches_screen(self.native(), self.oracle())
        e.ex("set relativenumber | normal! 25G")
        self.assert_matches_screen(self.native(), self.oracle())

    def test_fold_text_ignores_leftcol_while_guides_follow_it(self):
        e = self.editor
        e.ex("call setline(5, repeat('x', 300)) | 22,30fold | set colorcolumn=50 cursorcolumn")
        e.ex("normal! 5G100|zs")
        frame = self.native()
        window = self.window(frame)
        self.assertGreater(window["leftcol"], 0)
        screen = self.oracle()
        self.assert_matches_screen(frame, screen)
        fold_row = window["view"]["row"] + window["display_lines"].index(22)
        # Fold text starts at the text column; guides are shifted by leftcol.
        attrs = [cell[1] for cell in screen[fold_row]]
        cursor = 99 - window["leftcol"]
        self.assertNotEqual(attrs[cursor], attrs[cursor + 1])
        self.assertEqual(self.background(frame, fold_row, cursor), 0x11aa00)
        self.assertEqual(self.background(frame, fold_row, cursor + 1), 0x334455)

    def test_folded_style_cursorline_and_visual_follow_vim(self):
        e = self.editor
        e.ex("set number cursorline | 22,30fold | normal! 25Gzt")
        frame = self.native()
        window = self.window(frame)
        row = window["view"]["row"]
        screen = self.oracle()
        self.assertEqual(self.background(frame, row, 0), 0x334455)
        self.assertEqual(self.background(frame, row, 60), 0x334455, "Folded covers the whole row")
        e.ex("set nocursorline")
        plain = self.oracle()
        self.assertEqual(screen[row][10][1], plain[row][10][1], "Vim draws no CursorLine on a closed fold")
        self.assertEqual(frame["cursor"]["row"], row)
        e.ex("normal! 20G")
        e.keys("Vjjjjjjjjjjjj")
        e.state_where(lambda s: s["mode"] == "V")
        frame = self.native()
        fold_row = window["view"]["row"] + self.window(frame)["display_lines"].index(22)
        self.assertEqual(self.background(frame, fold_row, 40), 0x00aaaa)
        self.assertEqual(self.background(frame, fold_row, 1), 0x334455, "Visual starts at the text column")
        self.assertEqual(self.background(frame, fold_row, 4), 0x00aaaa)
        e.keys(b"\x1b")
        e.state_where(lambda s: s["mode"] == "n")
        # Characterwise: Vim highlights the fold only when the selection
        # covers it from column one through its last line's end.
        for keys, selected in [("vjj", None), ("vjjj", None), ("0vjjj$", True)]:
            e.ex("normal! 20G05l" if keys.startswith("v") else "normal! 22G")
            if not keys.startswith("v"):
                e.keys("k")
            e.keys(keys)
            e.state_where(lambda s: s["mode"] == "v")
            frame = self.native()
            screen = self.oracle()
            vim_selected = screen[fold_row][40][1] != plain[fold_row][40][1]
            if selected is not None:
                self.assertEqual(vim_selected, selected)
            self.assertEqual(self.background(frame, fold_row, 40) == 0x00aaaa, vim_selected, keys)
            e.keys(b"\x1b")
            e.state_where(lambda s: s["mode"] == "n")
        e.keys(b"\x16jj")
        e.state_where(lambda s: s["mode"] == "\x16")
        frame = self.editor.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertIn("Block selections", frame["fallback_reason"])

    def test_relative_numbers_count_display_rows_and_hybrid_is_left_aligned(self):
        e = self.editor
        e.ex("set number relativenumber numberwidth=6 | normal! 20Gzt")
        frame = self.native()
        self.assert_matches_screen(frame, self.oracle())
        label = next(r["text"] for r in frame["runs"] if r["row"] == 0 and r["column"] == 0)
        self.assertEqual(label, "20    ", "Vim left-aligns the hybrid cursor number")
        e.ex("22,30fold | normal! 20G")
        frame = self.native()
        self.assert_matches_screen(frame, self.oracle())
        labels = {r["row"]: r["text"] for r in frame["runs"] if r["column"] == 0 and r["text"].strip()}
        self.assertEqual(labels[3].strip(), "3")
        self.assertEqual(labels[4].strip(), "4", "Line 32 is four display rows below line 20")

    def test_fold_toggles_and_inactive_splits_repaint_without_buffer_changes(self):
        e = self.editor
        e.ex("22,30fold | normal! 20Gzt")
        frame = self.native()
        tick = frame["changedtick"]
        self.assertIn(30, self.window(frame)["fold_ends"])
        e.ex("normal! 22Gzo")
        frame = self.native()
        self.assertEqual(frame["changedtick"], tick)
        self.assertNotIn("display_lines", self.window(frame))
        self.assert_matches_screen(frame, self.oracle())
        e.ex("normal! zc")
        e.ex("vsplit | wincmd p | normal! 25G")
        frame = self.native()
        self.assertEqual(len(frame["windows"]), 2)
        screen = self.oracle()
        for window in frame["windows"]:
            self.assertIn("fold_ends", window)
            self.assert_matches_screen(frame, screen, window["window"])

    def test_scroll_view_counts_a_closed_fold_as_one_row(self):
        e = self.editor
        e.ex("22,300fold | normal! 18Gzt")
        frame = self.native()
        window = self.window(frame)
        below = [edge for edge in window["overscan"] if edge["row"] >= window["view"]["rows"]]
        rows = window["view"]["rows"]
        self.assertEqual(window["display_lines"][:5], [18, 19, 20, 21, 22])
        self.assertEqual(window["display_lines"][5], 301)
        self.assertEqual([edge["line"] for edge in below], list(range(301 + rows - 5, 309 + rows - 5)))
        e.request("scroll_view", window=window["window"], buffer=window["buffer"],
                  changedtick=window["changedtick"], topline=18, leftcol=0, lines=5)
        frame = self.native()
        window = self.window(frame)
        self.assertEqual(window["topline"], 301)
        self.assertEqual(window["scroll_rows"], 5)
        self.assertEqual(window["scroll_ack"] > 0, True)
        above = {edge["row"]: edge for edge in window["overscan"] if edge["row"] < 0}
        self.assertEqual((above[-1]["line"], above[-1]["fold_end"]), (22, 300))
        self.assertEqual(above[-2]["line"], 21)
        e.request("scroll_view", window=window["window"], buffer=window["buffer"],
                  changedtick=window["changedtick"], topline=301, leftcol=0, lines=-2)
        window = self.window(self.native())
        self.assertEqual((window["topline"], window["scroll_rows"]), (21, -2))

    def test_properties_inside_a_closed_fold_do_not_consume_the_paint_budget(self):
        e = self.editor
        e.ex("call prop_type_add('hidden', {'highlight': 'Visual'})")
        e.ex("for l in range(96, 150) | for c in range(1, 100) | call prop_add(l, 1, {'type': 'hidden', 'length': 1}) | endfor | endfor")
        e.ex("normal! 95Gzt")
        frame = self.editor.gui_frame()
        self.assertFalse(frame["supported"], "The unfolded view exceeds the property budget")
        e.ex("96,300fold | normal! 90Gzt")
        frame = self.native()
        self.assert_matches_screen(frame, self.oracle())

    def test_older_protocols_keep_the_compatibility_view_for_folds(self):
        self.editor.close()
        self.editor = Editor(self.root, authenticate=False)
        self.connect(7)
        e = self.editor
        self.install_oracle()
        e.ex("call setline(1, map(range(1, 100), 'string(v:val)')) | 5,8fold | normal! gg")
        frame = e.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertEqual(frame["fallback_reason"], "Closed Vim folds use the compatibility view")
        e.ex("normal! 50Gzt")
        frame = e.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        window = frame["windows"][0]
        self.assertNotIn("display_lines", window)
        self.assertNotIn("scroll_rows", window)



def vimrc_paints_folds() -> bool:
    core = VIMRC_ROOT / "config" / "core.vim"
    return core.is_file() and "VimrcFoldText" in core.read_text(encoding="utf-8")


@unittest.skipUnless(VIM and vimrc_paints_folds(), "Needs a vimrc checkout with native fold support")
class VimrcFoldTests(unittest.TestCase):
    """The shipped vimrc keeps folds on only for hosts that paint them."""

    def editor(self, protocol):
        directory = tempfile.TemporaryDirectory(prefix="vim9-vimrc-folds-")
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        (root / "home").mkdir()
        vimrc = "execute 'source ' .. fnameescape('%s')\nset nomore\n" % str(VIMRC_ROOT / ".vimrc").replace("'", "''")
        e = Editor(root, authenticate=False, vimrc_extra=vimrc, extra_env={
            "HOME": str(root / "home"), "XDG_STATE_HOME": str(root / "state"),
            "XDG_CONFIG_HOME": str(root / "config"), "VIMRC_SKIP_PLUGINS": "1",
            "VIMRC_SKIP_UPDATE_CHECK": "1"})
        self.addCleanup(e.close)
        e.send({"action": "auth", "token": e.token, "protocol": 1, "render_protocol": protocol})
        e.wait(lambda message: message.get("event") == "state")
        e.ex("call setline(1, ['fn main() {', '    let value = 1;', '    value', '}'] + map(range(40), 'string(v:val)'))")
        return e

    def test_protocol_8_paints_the_vimrc_fold_text(self):
        e = self.editor(8)
        e.ex("1,4fold | normal! gg")
        frame = e.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        window = frame["windows"][0]
        self.assertEqual(window["fold_ends"][0], 4)
        text = "".join(r["text"] for r in sorted(
            (r for r in frame["runs"] if r["row"] == window["view"]["row"] and r["line"] == 0),
            key=lambda r: r["column"]))
        self.assertIn("fn main() { \u22ef 4 lines", text)
        self.assertTrue(text.rstrip().endswith("\u00b7"), "vimrc fillchars fold glyph fills the row")

    def test_older_hosts_turn_folding_off_at_connect(self):
        e = self.editor(7)
        e.ex("1,4fold | normal! gg")
        frame = e.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        self.assertNotIn("fold_ends", frame["windows"][0])


if __name__ == "__main__":
    unittest.main()
