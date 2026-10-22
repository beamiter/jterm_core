#!/usr/bin/env python3
"""Bounded listchars painting compared with Vim's actual screen oracle."""

import json
from pathlib import Path
import tempfile
import unicodedata
import unittest

from test_bridge import VIM
from test_render_wincolor import WindowColorEditor


@unittest.skipUnless(VIM, "Install Vim 9 for native listchars tests")
class RenderListcharsTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-listchars-")
        self.root = Path(self.directory.name)
        self.editor = WindowColorEditor(self.root)
        self.original = "  a  \n\ta\t\n   \na\u00a0b\u202fc\na\tb\n\n界é x \n"
        self.editor.file.write_text(self.original)
        e = self.editor
        e.ex("edit! | set nonumber norelativenumber nocursorline nocursorcolumn nowrap tabstop=4 mouse=a")
        for group, fg, bg, termfg, termbg in [
            ("Normal", "eeeeee", "20242c", 15, 0),
            ("SpecialKey", "cc3344", "NONE", 1, "NONE"),
            ("NonText", "33cc44", "NONE", 2, "NONE"),
            ("Search", "eeeeee", "3344cc", 15, 4),
            ("Visual", "eeeeee", "775511", 15, 3),
            ("ColorColumn", "NONE", "114466", "NONE", 6),
        ]:
            e.ex(f"highlight {group} guifg={'NONE' if fg == 'NONE' else '#' + fg} "
                 f"guibg={'NONE' if bg == 'NONE' else '#' + bg} ctermfg={termfg} ctermbg={termbg} gui=NONE cterm=NONE")
        e.ex('let &listchars="tab:»·+,trail:·,space:␣,lead:.,nbsp:%,eol:↲" | set list')
        command = ("redraw!<Bar>call writefile([json_encode({'rows':map(range(0,6), "
                   "{_,r -> join(map(range(0,31), {_,c -> screenstring(getwininfo(win_getid())[0].winrow+r, "
                   "getwininfo(win_getid())[0].wincol+c)}),'')}), 'attrs':map(range(0,6), "
                   "{_,r -> map(range(0,31), {_,c -> screenattr(getwininfo(win_getid())[0].winrow+r, "
                   "getwininfo(win_getid())[0].wincol+c)})})})], 'oracle.json')")
        e.ex("nnoremap <F8> <Cmd>" + command + "<CR>")
        e.ex("xnoremap <F8> <Cmd>" + command + "<CR>")

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def native(self):
        frame = self.editor.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        return frame

    def window(self, frame, window=None):
        return next(w for w in frame["windows"] if w["window"] == (window or frame["window"]))

    def row(self, frame, line, window=None):
        w = self.window(frame, window)
        cells = [" "] * w["view"]["columns"]
        row = w["view"]["row"] + line - w["topline"]
        for run in frame["runs"]:
            if run["window"] != w["window"] or run["row"] != row:
                continue
            clusters = []
            for char in run["text"]:
                if unicodedata.combining(char) and clusters:
                    clusters[-1] += char
                else:
                    clusters.append(char)
            col = run["column"] - w["view"]["column"]
            for cluster in clusters:
                width = 2 if unicodedata.east_asian_width(cluster[0]) in ("W", "F") else 1
                if 0 <= col < len(cells):
                    cells[col] = cluster
                    for continuation in range(col + 1, min(col + width, len(cells))):
                        cells[continuation] = ""
                col += width
        return "".join(cells).rstrip()

    def style(self, frame, line, column):
        w = self.window(frame)
        row = w["view"]["row"] + line - w["topline"]
        col = w["view"]["column"] + column
        return next((r["style"] for r in reversed(frame["runs"])
                     if r["window"] == w["window"] and r["row"] == row
                     and r["column"] <= col < r["column"] + r["cells"]), frame["default_style"])

    def oracle(self):
        path = self.root / "oracle.json"
        path.unlink(missing_ok=True)
        self.editor.keys(b"\x1b[19~")
        for _ in range(100):
            self.editor.pump(0.02)
            try:
                return json.loads(path.read_text())
            except (FileNotFoundError, json.JSONDecodeError):
                pass
        self.fail("Vim did not finish the screen oracle")

    def test_real_screen_glyphs_colors_and_original_source_bytes(self):
        frame = self.native()
        expected = ["..a··↲", "»··+a»·+↲", "···↲", "a%b%c↲", "a»·+b↲", "↲", "界é␣x·↲"]
        actual = self.oracle()
        for line, text in enumerate(expected, 1):
            self.assertEqual(self.row(frame, line), text)
            self.assertEqual(actual["rows"][line - 1].rstrip(), text)
        self.assertEqual(self.style(frame, 1, 0)["foreground"], 0xcc3344)
        self.assertEqual(self.style(frame, 1, 5)["foreground"], 0x33cc44)
        tabs = [r for r in frame["runs"] if r["line"] == 2 and r["byte_start"] == 0 and r["byte_end"] == 1]
        self.assertEqual([r["text"] for r in tabs], list("»··+"))
        self.assertTrue(all(r["cells"] == 1 for r in tabs))
        nbsp = [r for r in frame["runs"] if r["line"] == 4 and r["text"] == "%"]
        self.assertEqual([(r["byte_start"], r["byte_end"]) for r in nbsp], [(1, 3), (4, 7)])
        eol = [r for r in frame["runs"] if r["text"] == "↲"]
        self.assertTrue(all(r["line"] == 0 and r["byte_start"] == r["byte_end"] == 0 for r in eol))
        self.assertFalse(self.editor.snapshot()["modified"])
        self.editor.request("save")
        self.assertEqual(self.editor.file.read_text(), self.original)

    def test_tab_cursor_and_source_pointer_keep_tab_and_wide_cells_distinct(self):
        e = self.editor
        w = self.window(self.native())
        source = {key: w[key] for key in ("window", "buffer", "changedtick")}
        e.request("pointer", **source, phase="press", gesture=1, line=2, byte_column=0)
        e.request("pointer", **source, phase="release", gesture=1, line=2, byte_column=0)
        self.assertEqual(self.native()["cursor"]["width"], 1)
        e.request("goto", line=7, column=1)
        self.assertEqual(self.native()["cursor"]["width"], 2)
        self.assertEqual(e.file.read_text(), self.original)

    def test_hex_escapes_and_one_cell_three_character_tabs_match_vim(self):
        e = self.editor
        e.ex('enew! | call setline(1,["abc\\t","ab\\t","a\\t"]) | setlocal list tabstop=4')
        e.ex(r"let &l:listchars='tab:\u00ab\x2d\u00bb,eol:\u21b5'")
        frame = self.native()
        raw = self.oracle()
        for line, expected in enumerate(["abc»↵", "ab«»↵", "a«-»↵"], 1):
            self.assertEqual(self.row(frame, line), expected)
            self.assertEqual(raw["rows"][line - 1].rstrip(), expected)
        e.ex('let &l:listchars="tab:>-,eol:$"')
        frame = self.native()
        raw = self.oracle()
        for line, expected in enumerate(["abc>$", "ab>-$", "a>--$"], 1):
            self.assertEqual(self.row(frame, line), expected)
            self.assertEqual(raw["rows"][line - 1].rstrip(), expected)

    def test_search_visual_and_column_priority_preserve_marker_text(self):
        e = self.editor
        e.ex("let @/ = '  ' | set hlsearch colorcolumn=2")
        frame = self.native()
        raw = self.oracle()
        self.assertEqual(self.style(frame, 1, 1)["background"], 0x3344cc)
        self.assertEqual(self.style(frame, 1, 1)["foreground"], 0xeeeeee)
        self.assertEqual(raw["attrs"][0][0], raw["attrs"][0][1])
        e.ex("nohlsearch")
        self.assertEqual(self.style(self.native(), 1, 1)["background"], 0x114466)
        e.keys("gg0vl")
        e.state_where(lambda s: s["mode"] == "v")
        frame = self.native()
        raw = self.oracle()
        self.assertEqual(self.style(frame, 1, 1)["background"], 0x775511)
        self.assertEqual(self.style(frame, 1, 1)["foreground"], 0xeeeeee)
        self.assertEqual(self.row(frame, 1), raw["rows"][0].rstrip())
        e.keys(b"\x1b")
        e.keys("gg0vj")
        e.state_where(lambda s: s["mode"] == "v")
        frame = self.native()
        raw = self.oracle()
        self.assertEqual(self.row(frame, 1), raw["rows"][0].rstrip())
        self.assertEqual(self.style(frame, 1, 5)["background"], 0x775511)

    def test_window_local_option_changes_invalidate_without_source_edits(self):
        e = self.editor
        first = self.native()
        original = first["window"]
        e.request("split", vertical=True)
        e.ex('let &l:listchars="tab:>-,space:_,eol:$"')
        frame = self.native()
        self.assertEqual(frame["changedtick"], first["changedtick"])
        self.assertEqual(self.row(frame, 1, original), "..a··↲")
        self.assertEqual(self.row(frame, 1), "__a__$")
        e.ex('nnoremap <F9> <Cmd>let &l:listchars="tab:>-,space:+,eol:#"<CR>')
        frame = self.native()
        e.keys(b"\x1b[20~")
        changed = e.painted(lambda f: f["revision"] > frame["revision"] and self.row(f, 1) == "++a++#")
        self.assertEqual(changed["changedtick"], first["changedtick"])
        self.assertEqual(self.row(changed, 1, original), "..a··↲")

    def test_horizontal_clipped_tab_markers_and_margin_rows_match_vim(self):
        e = self.editor
        text = "\t" * 60 + " x "
        e.file.write_text((text + "\n") * 80)
        e.ex("edit! | vsplit | setlocal nolist | wincmd p | vertical resize 40 | call cursor(20,10) | call winrestview({'topline':20,'leftcol':18})")
        frame = self.native()
        window = self.window(frame)
        self.assertGreater(window["leftcol"], 0)
        raw = self.oracle()
        self.assertEqual(self.row(frame, window["topline"])[:32], raw["rows"][0].rstrip())
        self.assertTrue(window["overscan"])
        for edge in window["overscan"]:
            markers = [r for r in edge["runs"] if r["byte_end"] > r["byte_start"]]
            self.assertTrue(markers)
            self.assertTrue(all(r["cells"] == 1 and r["byte_end"] - r["byte_start"] == 1
                                for r in markers))
        self.assertEqual(e.file.read_text(), (text + "\n") * 80)

    def test_unsupported_modes_are_explicit_and_never_change_list_options(self):
        e = self.editor
        for value, reason in [("eol:$", "^I"), ("multispace:--", "field")]:
            e.ex("let &l:listchars='" + value + "'")
            frame = e.gui_frame()
            self.assertFalse(frame["supported"])
            self.assertIn(reason, frame["fallback_reason"])
        e.ex("enew! | call setline(1,'short') | setlocal list listchars=eol:$,extends:>,precedes:<")
        self.native()
        e.ex("call setline(1,repeat('x',300))")
        self.assertIn("edge marker", e.gui_frame()["fallback_reason"])
        e.ex("setlocal listchars=eol:$ wrap | call setline(1,repeat('x',120))")
        frame = e.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertIn("wrapped listchars EOL", frame["fallback_reason"])
        raw = self.oracle()
        self.assertTrue(raw["rows"][1].startswith("$"))


if __name__ == "__main__":
    unittest.main()
