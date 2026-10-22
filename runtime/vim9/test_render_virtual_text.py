#!/usr/bin/env python3
"""Vim-owned inline/after virtual labels, compared with the real screen and input.

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
        for options in ({"text_align": "right"},
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
        self.assertEqual(self.read_json("[g:jterm_vim9_connected, g:jterm_vim9_virtual_text_after, g:jterm_vim9_virtual_text_inline]"), [True, True, True])
        e.ex("autocmd User JTermVim9Disconnected call writefile([json_encode([g:jterm_vim9_connected, g:jterm_vim9_virtual_text_after, g:jterm_vim9_virtual_text_inline])], 'disconnected.json')")
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
        self.assertEqual(observed, [False, False, False])

    def test_inline_order_tabs_unicode_and_saved_source(self):
        e = self.editor
        original = self.read_json("getline(1,'$')")
        before = self.native()
        self.add(1, " XX", column=2)
        self.add(1, " YY", column=2)
        self.add(1, " ZZ", kind="Lens", column=2)
        self.add(2, "HI", column=2)
        self.add(2, "Q", column=3)
        self.add(3, "空 é", column=1)
        frame = self.native()
        self.assertEqual(self.row(frame, 1), "a ZZ XX YYbc")
        self.assertEqual(self.row(frame, 2), "aHI Q界é")
        self.assert_screen(frame)
        source = [r for r in frame["runs"] if r["line"] == 2 and r["byte_end"] > r["byte_start"]]
        tab = next(r for r in source if r["byte_start"] == 1)
        self.assertEqual((tab["text"], tab["cells"], tab["byte_end"]), (" ", 1, 2))
        wide = next(r for r in source if r["text"] == "界")
        self.assertEqual((wide["column"], wide["byte_start"], wide["byte_end"]), (5, 2, 5))
        labels = [r for r in frame["runs"] if r["style"]["foreground"] in (0x12ab34, 0xab1234)]
        self.assertTrue(labels)
        self.assertTrue(all((r["line"], r["byte_start"], r["byte_end"]) == (0, 0, 0) for r in labels))
        self.assertEqual(frame["changedtick"], before["changedtick"])
        e.request("save")
        self.assertEqual(e.file.read_text(), "\n".join(original) + "\n")

    def test_inline_eol_labels_listchars_and_guides_match_vim(self):
        e = self.editor
        e.ex("highlight CursorLine guibg=#445566 gui=NONE ctermbg=4 cterm=NONE | set cursorline")
        e.ex("highlight ColorColumn guibg=#556677 gui=NONE ctermbg=6 cterm=NONE | set colorcolumn=3")
        e.ex("set list listchars=tab:>-,eol:$ | normal! gg0")
        self.add(1, "AB", column=2)
        self.add(1, "END", column=4)
        self.add(1, " tail", kind="Lens")
        self.add(2, "X", column=2)
        frame = self.native()
        screen = self.assert_screen(frame)
        self.assertEqual(self.style(frame, 1, 1)["background"], 0x445566)
        self.assertEqual(self.style(frame, 1, 2)["background"], 0x556677)
        self.assertNotEqual(screen["attrs"][0][1], screen["attrs"][0][2])
        e.ex("normal! 2|")
        frame = self.native()
        self.assertEqual(frame["cursor"]["column"], 3)
        self.assertEqual(frame["cursor"]["byte_column"], 1)

    def test_inline_insertion_caret_and_undo_keep_hint_out_of_buffer(self):
        e = self.editor
        self.add(1, "type: ", column=1)
        before = self.native()
        self.assertEqual(before["cursor"]["column"], 6)
        e.keys("i")
        e.state_where(lambda s: s["mode"] == "i")
        inserted = self.native()
        self.assertEqual((inserted["cursor"]["column"], inserted["cursor"]["width"]), (0, 1))
        self.assertTrue(self.window(inserted)["native_scroll"])
        e.keys("!")
        e.state_where(lambda s: s["changedtick"] > before["changedtick"])
        edited = self.native()
        self.assertEqual(self.row(edited, 1), "!type: abc")
        e.request("save")
        self.assertEqual(e.file.read_text().splitlines()[0], "!abc")
        e.keys(b"\x1bu")
        e.state_where(lambda s: s["mode"] == "n" and s["changedtick"] > edited["changedtick"])
        self.assert_screen(self.native())
        e.request("save")
        self.assertEqual(e.file.read_text().splitlines()[0], "abc")
        e.ex("call prop_clear(1)")
        self.add(1, "END", column=4)
        e.keys("A")
        e.state_where(lambda s: s["mode"] == "i")
        frame = self.native()
        self.assertEqual((frame["cursor"]["column"], frame["cursor"]["width"]), (3, 1))

    def test_inline_mouse_is_vim_owned_and_removal_restores_native_pointer(self):
        e = self.editor
        e.ex("set mouse=a ttymouse=sgr")
        self.add(1, " parameter: ", column=2)
        frame = self.native()
        self.assertFalse(self.window(frame)["native_pointer"])
        self.assertTrue(self.window(frame)["native_scroll"])
        active = self.window(frame)
        e.request("pointer", window=active["window"], buffer=active["buffer"],
                  changedtick=active["changedtick"], gesture=1, phase="press",
                  line=1, byte_column=1, ok=False)
        # Click inside the hint. Vim resolves it to the following source byte.
        e.keys("\x1b[<0;6;1M\x1b[<0;6;1m")
        e.state_where(lambda s: s["line"] == 1 and s["column"] == 2)
        self.assertEqual(e.snapshot()["changedtick"], frame["changedtick"])
        e.ex("call prop_clear(1)")
        restored = self.native()
        self.assertTrue(self.window(restored)["native_pointer"])
        self.assertEqual(self.row(restored, 1), "abc")
        self.add(1, "hint", column=2)
        e.ex("vsplit other.txt | wincmd p")
        split = self.native()
        self.assertFalse(self.window(split)["native_pointer"])
        peer = next(w for w in split["windows"] if w["window"] != split["window"])
        self.assertTrue(peer["native_pointer"], "An unrelated split retains source pointer input")
        for phase in ("press", "release"):
            e.request("pointer", window=peer["window"], buffer=peer["buffer"],
                      changedtick=peer["changedtick"], gesture=2, phase=phase,
                      line=1, byte_column=0)
        self.assertEqual(e.snapshot()["window"], peer["window"])

    def test_inline_horizontal_clipping_uses_shifted_source_columns(self):
        e = self.editor
        e.ex("call setline(1, repeat('a', 140))")
        self.add(1, "01234567890123456789", column=61)
        e.ex("call cursor(1,130) | call winrestview({'leftcol':65})")
        frame = self.native()
        self.assertGreater(self.window(frame)["leftcol"], 0)
        self.assert_screen(frame)
        cursor = frame["cursor"]
        source = next(r for r in frame["runs"] if r["line"] == 1
                      and r["byte_start"] <= cursor["byte_column"] < r["byte_end"])
        self.assertEqual(source["column"] + cursor["byte_column"] - source["byte_start"], cursor["column"])

    def test_inline_split_margins_and_source_scroll_keep_window_identity(self):
        e = self.editor
        e.ex("call setline(1, map(range(1,80), {_,n -> 'source ' . n}))")
        e.ex("for n in range(1,80) | call prop_add(n,4,{'type':'Hint','text':':type '}) | endfor")
        e.request("split", vertical=True)
        e.ex("normal! 20Gzt")
        frame = self.native()
        active = self.window(frame)
        self.assert_screen(frame)
        self.assertTrue(active["native_scroll"])
        self.assertFalse(active["native_pointer"])
        self.assertTrue(any(r["line"] == 0 and ":type " in r["text"]
                            for row in active["overscan"] for r in row["runs"]))
        peer = next(w for w in frame["windows"] if w["window"] != active["window"])
        e.request("scroll_view", window=active["window"], buffer=active["buffer"],
                  changedtick=active["changedtick"], topline=active["topline"],
                  leftcol=active["leftcol"], lines=3)
        scrolled = self.native()
        self.assertEqual(self.window(scrolled)["topline"], active["topline"] + 3)
        self.assertEqual(self.window(scrolled, peer["window"])["topline"], peer["topline"])
        self.assert_screen(scrolled)

    def test_inline_wrap_visual_and_partial_graphemes_fall_back_and_recover(self):
        e = self.editor
        self.add(1, "hint", column=2)
        e.ex("set wrap")
        frame = e.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertIn("Inline Vim virtual text with wrap", frame["fallback_reason"])
        self.assertTrue(self.read_json("&wrap"))
        e.ex("set nowrap")
        self.native()
        e.keys("v")
        e.state_where(lambda s: s["mode"] == "v")
        self.assertFalse(e.gui_frame()["supported"])
        e.keys(b"\x1b")
        e.state_where(lambda s: s["mode"] == "n")
        self.native()
        e.ex("call prop_clear(1,5)")
        self.add(2, "bad", column=4)  # Inside the UTF-8 bytes of 界.
        frame = e.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertIn("grapheme", frame["fallback_reason"])
        e.ex("call prop_clear(1,5) | call setline(1, repeat('a',119) . 'z')")
        self.add(1, "界", column=120)
        frame = e.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertIn("Partially clipped", frame["fallback_reason"])
        e.ex("call prop_clear(1,5)")
        self.native()

    def test_inline_property_only_replacement_and_clear_update_exact_delta_row(self):
        e = self.editor
        e.ex("call setline(1, map(range(1,80), {_,n -> 'source row ' . n}))")
        script = self.root / "inline-provider.vim"
        script.write_text(
            "let g:inline_phase = 0\n"
            "function! UpdateInline(timer)\n"
            "  call prop_clear(2)\n"
            "  if g:inline_phase < 2\n"
            "    call prop_add(2, 8, {'type':'Hint','text': g:inline_phase == 0 ? 'xx' : 'long hint'})\n"
            "  endif\n"
            "  let g:inline_phase += 1\n"
            "  doautocmd <nomodeline> User SimpleCCDecorationsChanged\n"
            "endfunction\n"
            "nnoremap <F10> <Cmd>call timer_start(80, 'UpdateInline')<CR>\n")
        e.ex("source " + str(script))
        e.ex("for t in timer_info() | if t.time == 750 && string(t.callback) =~# '_Poll' | call timer_stop(t.id) | endif | endfor")
        before = self.native()
        for text in ("xx", "long hint", ""):
            revision = e.paint["revision"]
            e.keys(b"\x1b[21~")
            expected = "source " + text + "row 2"
            frame = e.painted(lambda f: f["revision"] > revision and f["supported"]
                              and self.row(f, 2) == expected)
            self.assertEqual(frame["changedtick"], before["changedtick"])
            updates = [event for event in e.paint_events if event["event"] == "render_delta"
                       and event["revision"] > revision and event["row_updates"]]
            self.assertTrue(updates)
            self.assertEqual([row["row"] for row in updates[-1]["row_updates"]], [1])
            self.assertEqual(canonical(copy.deepcopy(e.paint)), canonical(self.native()))
            self.assertEqual(self.window(frame)["native_pointer"], not bool(text))


if __name__ == "__main__":
    unittest.main(verbosity=2)
