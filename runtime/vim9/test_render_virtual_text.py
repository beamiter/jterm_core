#!/usr/bin/env python3
"""Vim-owned after/EOL virtual labels, compared with the real screen and input.

No provider callback is needed here: public prop_add()/prop_list() are the
producer contract. Actual SimpleCC integration is covered separately.
"""

import copy
import json
from pathlib import Path
import tempfile
import time
import unicodedata
import unittest

from test_bridge import VIM
from test_render_delta import canonical
from test_render_popups import PopupEditor, vim_json


@unittest.skipUnless(VIM, "Install Vim 9 for virtual text integration tests")
class RenderVirtualTextTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-virtual-text-")
        self.root = Path(self.directory.name)
        self.editor = PopupEditor(self.root)
        e = self.editor
        e.ex("set nonumber norelativenumber nocursorline nocursorcolumn nowrap nolist mouse=a notermguicolors")
        e.ex("highlight Normal guifg=#eeeeee guibg=#20242c gui=NONE ctermfg=15 ctermbg=0 cterm=NONE")
        e.ex("highlight Hint guifg=#12ab34 guibg=NONE gui=italic ctermfg=2 ctermbg=NONE cterm=italic")
        e.ex("highlight Lens guifg=#ab1234 guibg=#334455 gui=bold ctermfg=1 ctermbg=3 cterm=bold")
        e.ex("call prop_type_add('Hint', {'highlight':'Hint', 'priority':1})")
        e.ex("call prop_type_add('Lens', {'highlight':'Lens', 'priority':10})")
        e.ex("call setline(1, ['abc', \"a\\t界é\", '', 'short', 'other']) | set tabstop=4")
        capture = (
            "redraw!<Bar>call writefile([json_encode({'rows':map(range(0,4), "
            "{_,r -> join(map(range(0,winwidth(0)-1), {_,c -> screenstring(getwininfo(win_getid())[0].winrow+r, "
            "getwininfo(win_getid())[0].wincol+c)}),'')}), 'attrs':map(range(0,4), "
            "{_,r -> map(range(0,winwidth(0)-1), {_,c -> screenattr(getwininfo(win_getid())[0].winrow+r, "
            "getwininfo(win_getid())[0].wincol+c)})})})], 'screen.json')")
        e.ex("nnoremap <F8> <Cmd>" + capture + "<CR>")
        self.native()

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def native(self):
        frame = self.editor.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        return frame

    def read_json(self, expression):
        self.editor.ex("call writefile([json_encode(" + expression + ")], 'oracle.json')")
        return json.loads((self.root / "oracle.json").read_text())

    def add(self, line, text, *, kind="Hint", column=0, **options):
        self.editor.ex(f"call prop_add({line}, {column}, " +
                       vim_json(dict(type=kind, text=text, **options)) + ")")

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
        column += w["view"]["column"]
        return next((r["style"] for r in reversed(frame["runs"])
                     if r["window"] == w["window"] and r["row"] == row
                     and r["column"] <= column < r["column"] + r["cells"]), frame["default_style"])

    def oracle(self):
        path = self.root / "screen.json"
        path.unlink(missing_ok=True)
        self.editor.keys(b"\x1b[19~")
        for _ in range(100):
            self.editor.pump(0.02)
            try:
                return json.loads(path.read_text())
            except (FileNotFoundError, json.JSONDecodeError):
                pass
        self.fail("Vim did not finish the virtual-text screen oracle")

    def assert_screen(self, frame, count=5):
        screen = self.oracle()
        top = self.window(frame)["topline"]
        for offset in range(count):
            self.assertEqual(self.row(frame, top + offset), screen["rows"][offset].rstrip())
        return screen

    def test_priority_creation_order_padding_unicode_and_source_immutability(self):
        before = self.native()
        original = self.read_json("getline(1,'$')")
        self.add(1, " first")
        self.add(1, " second")
        self.add(1, " lens", kind="Lens")
        self.add(2, "中文 é", text_padding_left=2)
        self.add(3, "empty line")
        frame = self.native()
        self.assertEqual(self.row(frame, 1), "abc lens first second")
        self.assertEqual(self.row(frame, 2), "a   界é  中文 é")
        self.assert_screen(frame)
        labels = [r for r in frame["runs"] if r["line"] == 0
                  and r["style"]["foreground"] in (0x12ab34, 0xab1234)]
        self.assertTrue(labels)
        self.assertTrue(all(r["byte_start"] == r["byte_end"] == 0 for r in labels))
        self.assertTrue(any(r["text"] == "中" and r["cells"] == 2 for r in labels))
        self.assertTrue(any(r["text"] == "é" and r["cells"] == 1 for r in labels))
        self.assertEqual([r for r in frame["runs"] if r["line"] > 0],
                         [r for r in before["runs"] if r["line"] > 0])
        self.assertEqual(frame["changedtick"], before["changedtick"])
        self.editor.request("save")
        self.assertEqual(self.editor.file.read_text(), "\n".join(original) + "\n")

    def test_list_eol_cursorline_and_colorcolumn_use_vim_composition(self):
        e = self.editor
        e.ex("highlight CursorLine guibg=#445566 gui=NONE ctermbg=4 cterm=NONE | set cursorline")
        e.ex("highlight ColorColumn guibg=#556677 gui=NONE ctermbg=6 cterm=NONE | set colorcolumn=6")
        e.ex("set list listchars=tab:>-,eol:$ | normal! gg0")
        self.add(1, "ABCDE")
        frame = self.native()
        self.assertEqual(self.row(frame, 1), "abc$ABCDE")
        screen = self.assert_screen(frame)
        self.assertEqual(self.style(frame, 1, 4)["background"], 0x445566)
        self.assertEqual(self.style(frame, 1, 5)["background"], 0x556677)
        self.assertEqual(self.style(frame, 1, 6)["background"], 0x445566)
        self.assertEqual(self.style(frame, 1, 5)["foreground"], 0x12ab34)
        self.assertNotEqual(screen["attrs"][0][4], screen["attrs"][0][5])
        self.assertEqual(screen["attrs"][0][4], screen["attrs"][0][6])
        self.add(4, "界Z")
        e.ex("set colorcolumn=8")
        frame = self.native()
        self.assert_screen(frame)
        self.assertEqual(self.style(frame, 4, 7)["background"], 0x20242c,
                         "A guide through a wide continuation must not split its glyph")

    def test_nowrap_truncation_and_horizontal_scroll_preserve_source_geometry(self):
        self.add(1, "x" * 180)
        frame = self.native()
        self.assertEqual(self.row(frame, 1), "abc" + "x" * 117)
        self.assert_screen(frame)
        e = self.editor
        e.ex("call setline(1, repeat('a', 140))")
        self.add(1, " HINT")
        e.ex("normal! 130|")
        frame = self.native()
        self.assertGreater(self.window(frame)["leftcol"], 0)
        self.assertTrue(self.row(frame, 1).endswith(" HINT"))
        self.assert_screen(frame)
        self.assertEqual(frame["cursor"]["byte_column"], 129)

    def test_insert_and_undo_move_labels_without_inserting_them_into_source(self):
        e = self.editor
        original = self.read_json("getline(1,'$')")
        self.add(1, " hint")
        before = self.native()
        e.keys("ggA!")
        e.state_where(lambda s: s["mode"] == "i" and s["changedtick"] > before["changedtick"])
        frame = self.native()
        self.assertEqual(self.row(frame, 1), "abc! hint")
        self.assertEqual(frame["cursor"]["byte_column"], 4)
        self.assertTrue(self.window(frame)["native_scroll"])
        e.request("save")
        self.assertEqual(e.file.read_text().splitlines()[0], "abc!")
        e.keys(b"\x1bu")
        e.state_where(lambda s: s["mode"] == "n" and s["changedtick"] > frame["changedtick"])
        frame = self.native()
        self.assertEqual(self.row(frame, 1), "abc hint")
        self.assert_screen(frame)
        e.request("save")
        self.assertEqual(e.file.read_text(), "\n".join(original) + "\n")

    def test_fallback_subsets_recover_without_changing_display_options(self):
        e = self.editor
        for options in ({"column": 2}, {"text_align": "right"},
                        {"text_align": "above"}, {"text_align": "below"}, {"text_wrap": "wrap"}):
            with self.subTest(options=options):
                self.add(1, "hint", **options)
                frame = e.gui_frame()
                self.assertFalse(frame["supported"])
                self.assertIn("virtual text", frame["fallback_reason"])
                e.ex("call prop_clear(1)")
                self.native()
        self.add(1, "hint")
        e.ex("set wrap")
        self.native()
        self.add(1, "x" * 130)
        frame = e.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertIn("Truncated wrapped", frame["fallback_reason"])
        self.assertTrue(self.read_json("&wrap"))
        e.ex("set nowrap | call prop_clear(1)")
        self.add(1, "hint")
        e.keys("V")
        e.state_where(lambda s: s["mode"] == "V")
        frame = e.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertIn("Visual selections", frame["fallback_reason"])
        e.keys(b"\x1b")
        e.state_where(lambda s: s["mode"] == "n")
        self.native()

    def test_partial_wide_clipping_and_budget_overflow_are_explicit(self):
        e = self.editor
        e.ex("call setline(1, repeat('a', 119))")
        self.add(1, "界")
        frame = e.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertIn("Partially clipped", frame["fallback_reason"])
        e.ex("call prop_clear(1)")
        self.add(1, "x" * 1025)
        frame = e.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertIn("paint budget", frame["fallback_reason"])
        e.ex("call prop_clear(1)")
        self.native()

    def test_provider_event_updates_and_clears_exact_delta_rows_without_poll(self):
        e = self.editor
        e.ex("call setline(1, map(range(1,80), {_,n -> 'source row ' . n}))")
        script = self.root / "provider.vim"
        script.write_text(
            "function! UpdateLabels(timer)\n"
            "  if empty(prop_list(2))\n"
            "    call prop_add(2, 0, {'type':'Hint','text':' diagnostic'})\n"
            "  else\n"
            "    call prop_clear(2)\n"
            "  endif\n"
            "  doautocmd <nomodeline> User SimpleCCDecorationsChanged\n"
            "endfunction\n"
            "nnoremap <F10> <Cmd>call timer_start(80, 'UpdateLabels')<CR>\n")
        e.ex("source " + str(script))
        e.ex("for t in timer_info() | if t.time == 750 && string(t.callback) =~# '_Poll' | call timer_stop(t.id) | endif | endfor")
        before = self.native()
        for visible in (True, False):
            start = e.paint["revision"]
            e.keys(b"\x1b[21~")
            frame = e.painted(lambda f: f["revision"] > start and f["supported"]
                              and any("diagnostic" in r["text"] for r in f["runs"]) == visible)
            self.assertEqual(frame["changedtick"], before["changedtick"])
            deltas = [event for event in e.paint_events if event["event"] == "render_delta"
                      and event["revision"] > start and event["row_updates"]]
            self.assertTrue(deltas)
            self.assertEqual([row["row"] for row in deltas[-1]["row_updates"]], [1])
            reconstructed = copy.deepcopy(e.paint)
            self.assertEqual(canonical(reconstructed), canonical(self.native()))

    def test_split_ownership_scroll_margins_and_clicks_keep_real_vim_state(self):
        e = self.editor
        e.ex("call setline(1, map(range(1,80), {_,n -> 'source ' . n}))")
        e.ex("for n in range(1,80) | call prop_add(n,0,{'type':'Hint','text':' lens'}) | endfor")
        e.request("split", vertical=True)
        e.ex("normal! 20Gzt")
        before = e.snapshot()
        frame = self.native()
        active = self.window(frame)
        peer = next(w for w in frame["windows"] if w["window"] != frame["window"])
        self.assertNotEqual(active["topline"], peer["topline"])
        self.assert_screen(frame)
        self.assertTrue(any(r["line"] == 0 and "lens" in r["text"]
                            for row in active["overscan"] for r in row["runs"]))
        after = e.snapshot()
        self.assertEqual([before[k] for k in ("window", "buffer", "line", "column", "changedtick")],
                         [after[k] for k in ("window", "buffer", "line", "column", "changedtick")])
        self.assertTrue(active["native_pointer"] and active["native_scroll"])
        # Vim documents an after-label click as an EOL click. The native
        # source route sends the same original byte boundary, never label text.
        line = active["topline"]
        length = active["line_lengths"][0]
        e.request("pointer", window=active["window"], buffer=active["buffer"],
                  changedtick=active["changedtick"], gesture=1, phase="press",
                  line=line, byte_column=length)
        e.request("pointer", window=active["window"], buffer=active["buffer"],
                  changedtick=active["changedtick"], gesture=1, phase="release",
                  line=line, byte_column=length)
        state = e.snapshot()
        self.assertEqual(state["line"], line)
        self.assertEqual(state["column"], length)
        self.assertEqual(state["changedtick"], before["changedtick"])

    def test_capability_is_authenticated_and_cleared_before_disconnect_hook(self):
        e = self.editor
        self.assertEqual(self.read_json("[g:jterm_vim9_connected, g:jterm_vim9_virtual_text_after]"), [True, True])
        e.ex("autocmd User JTermVim9Disconnected call writefile([json_encode([g:jterm_vim9_connected, g:jterm_vim9_virtual_text_after])], 'disconnected.json')")
        e.connection.close()
        e.connection = None
        path = self.root / "disconnected.json"
        deadline = time.monotonic() + 3
        observed = None
        while time.monotonic() < deadline:
            e.pump(0.02)
            try:
                observed = json.loads(path.read_text())
                break
            except (FileNotFoundError, json.JSONDecodeError):
                pass
        self.assertEqual(observed, [False, False])


if __name__ == "__main__":
    unittest.main(verbosity=2)
