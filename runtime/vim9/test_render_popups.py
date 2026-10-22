#!/usr/bin/env python3
"""Protocol 6 samples provider-owned popups from real Vim buffer/window APIs.

These are real bordered/padded Vim popups with the options used by SimpleCC
and SimpleGit. Vim keeps their input and lifetime; the receiver only paints.
"""

from pathlib import Path
import json
import tempfile
import unittest

from test_bridge import VIM
from test_render_delta import DeltaEditor, canonical


def vim_json(value):
    return "json_decode('" + json.dumps(value, ensure_ascii=False).replace("'", "''") + "')"


class PopupEditor(DeltaEditor):
    render_protocol = 6

    def authenticate(self):
        self.send({"action": "auth", "token": self.token, "protocol": 1,
                   "render_protocol": self.render_protocol})
        return self.wait(lambda m: m.get("event") == "state")


@unittest.skipUnless(VIM, "Install Vim 9 for native popup integration tests")
class RenderPopupTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-popups-")
        self.root = Path(self.directory.name)
        self.editor = PopupEditor(self.root)
        self.configure()

    def configure(self):
        e = self.editor
        e.ex("set nonumber norelativenumber nocursorline nocursorcolumn nowrap notermguicolors mouse=a")
        e.ex("highlight Normal guifg=#eeeeee guibg=#20242c gui=NONE ctermfg=15 ctermbg=0")
        e.ex("highlight Pmenu guifg=#12ab34 guibg=#334455 gui=NONE ctermfg=1 ctermbg=2")
        e.ex("highlight SimpleCCFloatBorder guifg=#bbaa77 guibg=#334455 gui=NONE ctermfg=3 ctermbg=4")
        e.ex("call setline(1, map(range(1, 80), {_,n -> 'source line ' . n})) | normal! 10Gzt")
        self.assertTrue(e.gui_frame()["supported"])

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def create(self, lines, options=None, *, name="p", registered=True, atcursor=False, after=""):
        function = "popup_atcursor" if atcursor else "popup_create"
        command = f"let g:{name} = {function}({vim_json(lines)}, {vim_json(options or {})})"
        if registered:
            command += f" | call setwinvar(g:{name}, 'jterm_native_popup', 1)"
        if after:
            command += " | " + after
        self.editor.ex(command)
        return self.editor.gui_frame()

    def popup(self, frame, index=0):
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        self.assertGreater(len(frame.get("popups", [])), index)
        popup = frame["popups"][index]
        for run in popup["runs"]:
            self.assertEqual((run["window"], run["line"], run["byte_start"], run["byte_end"]), (0, 0, 0, 0))
            self.assertGreaterEqual(run["row"], popup["view"]["row"])
            self.assertLess(run["row"], popup["view"]["row"] + popup["view"]["rows"])
            self.assertGreaterEqual(run["column"], popup["view"]["column"])
            self.assertLessEqual(run["column"] + run["cells"], popup["view"]["column"] + popup["view"]["columns"])
        self.assertTrue(all(not w["native_pointer"] and not w["native_scroll"] for w in frame["windows"]))
        return popup

    def read_json(self, expression):
        self.editor.ex("call writefile([json_encode(" + expression + ")], 'oracle.json')")
        return json.loads((self.root / "oracle.json").read_text())

    def test_hover_geometry_padding_unicode_and_gui_styles(self):
        options = {"border": [1, 1, 1, 1], "padding": [0, 1, 0, 1],
                   "borderchars": ["─", "│", "─", "│", "╭", "╮", "╯", "╰"],
                   "maxwidth": 80, "maxheight": 20, "close": "click", "moved": "any"}
        frame = self.create(["**symbol**", "界é documentation"], options, atcursor=True,
                            after="call win_execute(g:p, 'setlocal filetype=markdown | syntax match PopupWord /symbol/')")
        self.editor.ex("highlight PopupWord guifg=#e97845 gui=bold,italic ctermfg=5")
        frame = self.editor.gui_frame()
        popup = self.popup(frame)
        pos = self.read_json("popup_getpos(g:p)")
        self.assertEqual(popup["view"], {"row": pos["line"] - 1, "column": pos["col"] - 1,
                                         "rows": pos["height"], "columns": pos["width"]})
        self.assertEqual(popup["default_style"]["background"], 0x334455)
        self.assertTrue(any(r["text"] == "界" and r["cells"] == 2 for r in popup["runs"]))
        self.assertTrue(any(r["text"] == "é" and r["cells"] == 1 for r in popup["runs"]))
        word = next(r for r in popup["runs"] if r["text"] == "symbol")
        self.assertEqual(word["style"]["foreground"], 0xe97845)
        self.assertTrue(word["style"]["bold"] and word["style"]["italic"])
        self.assertTrue(any(r["text"] == "╭" and r["row"] == pos["line"] - 1 for r in popup["runs"]))
        self.assertEqual(self.read_json("win_getid()"), frame["window"])
        self.assertEqual(canonical(self.editor.gui_frame()), canonical(frame))

    def test_signature_and_diagnostic_provider_options(self):
        for kind, extra in [("signature", {"pos": "botleft", "line": "cursor-1", "close": "none", "moved": [0, 0, 0]}),
                            ("diagnostic", {"moved": "any", "highlight": "Normal", "borderhighlight": ["SimpleCCFloatBorder"]})]:
            with self.subTest(kind=kind):
                self.editor.ex("call popup_clear()")
                options = {"border": [1, 1, 1, 1], "padding": [0, 1, 0, 1], "maxwidth": 80,
                           "maxheight": 10, "borderchars": ["─", "│", "─", "│", "╭", "╮", "╯", "╰"], **extra}
                popup = self.popup(self.create(["fn foo(x: i32)", "parameter x"], options, atcursor=True))
                if kind == "diagnostic":
                    self.assertEqual(popup["default_style"]["background"], 0x20242c)
                    top = next(r for r in popup["runs"] if r["text"] == "╭")
                    self.assertEqual(top["style"]["foreground"], 0xbbaa77)

    def test_git_hunk_diff_syntax_and_text_properties(self):
        self.editor.ex("syntax enable | highlight diffAdded guifg=#45d279 guibg=NONE gui=NONE ctermfg=1")
        options = {"padding": [0, 1, 0, 1], "border": [1, 1, 1, 1],
                   "borderchars": ["─", "│", "─", "│", "┌", "┐", "┘", "└"], "moved": "any", "maxheight": 20}
        frame = self.create(["@@ -1 +1 @@", "-old", "+new 界"], options, atcursor=True,
                            after="call win_execute(g:p, 'setlocal filetype=diff')")
        popup = self.popup(frame)
        self.assertTrue(any("+new" in r["text"] and r["style"]["foreground"] == 0x45d279 for r in popup["runs"]))
        self.editor.ex("highlight PopupProperty guifg=#f0a145 gui=underline")
        self.editor.ex("call prop_type_add('PopupProperty', {'bufnr':winbufnr(g:p), 'highlight':'PopupProperty', 'priority':100}) | call prop_add(2, 2, {'bufnr':winbufnr(g:p), 'length':3, 'type':'PopupProperty'})")
        popup = self.popup(self.editor.gui_frame())
        old = next(r for r in popup["runs"] if r["text"] == "old")
        self.assertEqual(old["style"]["foreground"], 0xf0a145)
        self.assertTrue(old["style"]["underline"])

    def test_z_order_and_hiding_are_complete_replacements(self):
        self.create(["first"], {"line": 3, "col": 8, "zindex": 200}, name="p1")
        self.create(["second"], {"line": 3, "col": 8, "zindex": 40}, name="p2")
        frame = self.create(["third"], {"line": 3, "col": 8, "zindex": 200}, name="p3")
        ids = self.read_json("[g:p2, g:p3, g:p1]")
        self.assertEqual([p["id"] for p in frame["popups"]], ids)
        self.assertEqual([p["zindex"] for p in frame["popups"]], [40, 200, 200])
        # The screen is an oracle in this test only. The producer exclusively
        # samples buffer/window APIs. Equal zindex keeps the older popup above.
        self.editor.ex("redraw!")
        self.assertEqual(self.read_json("join(map(range(8, 12), {_,c -> screenstring(3,c)}), '')"), "first")
        self.editor.ex("call popup_hide(g:p1)")
        hidden = self.editor.gui_frame()
        self.assertEqual([p["id"] for p in hidden["popups"]], [ids[0], ids[1]])
        self.editor.ex("call popup_clear()")
        cleared = self.editor.gui_frame()
        self.assertEqual(cleared["popups"], [])
        self.assertTrue(all(w["native_scroll"] for w in cleared["windows"]))

    def test_vim_movement_closes_popup_with_explicit_delta_clear(self):
        first = self.create(["hover"], {"moved": "any", "border": [], "padding": [0, 1, 0, 1]}, atcursor=True)
        self.popup(first)
        self.editor.keys("j")
        cleared = self.editor.painted(lambda f: f["revision"] > first["revision"] and f["supported"] and f.get("popups") == [])
        self.assertTrue(any(m["event"] == "render_delta" and m.get("popups") == []
                            and m["revision"] > first["revision"] for m in self.editor.paint_events))
        self.assertEqual(canonical(cleared), canonical(self.editor.gui_frame()))

    def test_async_completion_documentation_coexists_and_clears(self):
        e = self.editor
        script = self.root / "docs.vim"
        script.write_text(
            "function! PopupDocs(timer)\n"
            "  if !pumvisible() | return | endif\n"
            "  let pos = pum_getpos()\n"
            "  let order = []\n"
            "  for z in [99, 100, 101]\n"
            "    let probe = popup_create(['ZZZZZ'], {'line':pos.row+1, 'col':pos.col+1, 'zindex':z})\n"
            "    redraw!\n"
            "    call add(order, join(map(range(pos.col+1,pos.col+5), {_,c -> screenstring(pos.row+1,c)}), ''))\n"
            "    call popup_close(probe)\n"
            "  endfor\n"
            "  call writefile([json_encode(order)], 'pum-order.json')\n"
            "  let g:doc = popup_create(['fn alpha()', 'documentation 界'], {'border':[1,1,1,1], 'padding':[0,1,0,1], 'maxwidth':60, 'maxheight':15, 'pos':'topleft', 'line':'cursor-1', 'col':'cursor+40', 'moved':'any', 'highlight':'Normal', 'borderhighlight':['SimpleCCFloatBorder']})\n"
            "  call setwinvar(g:doc, 'jterm_native_popup', 1)\n"
            "  doautocmd <nomodeline> User JTermVim9PopupChanged\n"
            "endfunction\n"
            "autocmd CompleteDone * call popup_clear()\n", encoding="utf-8")
        e.ex("source " + str(script))
        e.ex("set completeopt=menuone,noinsert,noselect pumheight=20 | call setline(10, '')")
        e.ex("highlight! link PmenuMatch Pmenu | highlight! link PmenuMatchSel PmenuSel")
        e.keys("A\x12=[complete(col('.'), [{'word':'alpha','kind':'f'}, {'word':'beta'}]), timer_start(60, 'PopupDocs'), ''][2]\r")
        frame = e.painted(lambda f: f.get("completion") is not None and bool(f.get("popups")))
        self.popup(frame)
        self.assertEqual(len(frame["popups"]), 1)
        # Test-only screen oracle: a popup at PUM's exact zindex paints on top.
        self.assertEqual(json.loads((self.root / "pum-order.json").read_text()),
                         ["alpha", "ZZZZZ", "ZZZZZ"])
        self.assertEqual(canonical(frame), canonical(e.gui_frame()))
        e.keys("\x0e\x19\x1b")
        closed = e.painted(lambda f: f["mode"] == "n" and f.get("completion") is None and f.get("popups") == [])
        self.assertTrue(closed["supported"])
        e.ex("update")
        self.assertEqual(e.file.read_text().splitlines()[9], "alpha")

    def test_open_popups_reject_late_source_pointer_and_scroll_requests(self):
        e = self.editor
        before = e.gui_frame()
        window = next(w for w in before["windows"] if w["window"] == before["window"])
        source = {key: window[key] for key in ("window", "buffer", "changedtick")}
        self.popup(self.create(["hover"], {"line": 5, "col": 8}))
        e.request("pointer", **source, phase="press", gesture=1, line=12, byte_column=0, ok=False)
        e.request("scroll_view", **source, topline=window["topline"], leftcol=window["leftcol"], lines=1, ok=False)
        after = e.snapshot()
        self.assertEqual(after["line"], 10)
        self.assertEqual(after["changedtick"], before["changedtick"])

    def test_unknown_masked_or_interactive_popups_fall_back_and_clear(self):
        cases = [({"line": 3, "col": 5}, False, "Unregistered"),
                 ({"line": 3, "col": 5, "mask": [[1, 2, 1, 1]]}, False, "Unregistered"),
                 ({"line": 3, "col": 5, "title": "menu"}, True, "decorated"),
                 ({"line": 3, "col": 5, "filter": "PopupPass"}, True, "Interactive"),
                 ({"line": 3, "col": 5, "cursorline": 1}, True, "Interactive"),
                 ({"line": 3, "col": 5, "close": "button"}, True, "decorated")]
        self.editor.ex("execute " + json.dumps("function! PopupPass(id, key)\n  return 0\nendfunction"))
        for options, registered, expected in cases:
            with self.subTest(options=options):
                self.editor.ex("call popup_clear()")
                frame = self.create(["text"], options, registered=registered)
                self.assertFalse(frame["supported"])
                self.assertIn(expected, frame["fallback_reason"])
                self.assertEqual(frame["popups"], [])
                self.assertIsNone(frame["completion"])
        self.editor.ex("call popup_clear()")
        self.assertTrue(self.editor.gui_frame()["supported"])

    def test_wrapping_scrolling_and_virtual_text_fall_back(self):
        for lines, options in [(["123456789012345"], {"line": 3, "col": 5, "maxwidth": 6}),
                               (["a", "b", "c"], {"line": 3, "col": 5, "maxheight": 1}),
                               (["a", "b", "c"], {"line": 3, "col": 5, "firstline": 2})]:
            with self.subTest(options=options):
                self.editor.ex("call popup_clear()")
                frame = self.create(lines, options)
                self.assertFalse(frame["supported"])
                self.assertEqual(frame["popups"], [])
        self.editor.ex("call popup_clear()")
        self.create(["text"], {"line": 3, "col": 5})
        self.editor.ex("call prop_type_add('PopupVirtual', {'bufnr':winbufnr(g:p), 'highlight':'Normal'}) | call prop_add(1, 0, {'bufnr':winbufnr(g:p), 'text':'hint', 'type':'PopupVirtual'})")
        frame = self.editor.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertEqual(frame["popups"], [])

    def test_legacy_protocol_keeps_registered_popup_in_compatibility(self):
        self.editor.close()
        class LegacyEditor(PopupEditor):
            render_protocol = 5
        self.editor = LegacyEditor(self.root)
        self.configure()
        frame = self.create(["hover"], {"line": 3, "col": 5, "border": []})
        self.assertFalse(frame["supported"])
        self.assertIn("popup", frame["fallback_reason"])
        self.assertEqual(frame["popups"], [])

    def test_surface_count_budget_fails_closed(self):
        self.editor.ex("for n in range(1, 9) | let id = popup_create(['text'], {'line':n, 'col':n}) | call setwinvar(id,'jterm_native_popup',1) | endfor")
        frame = self.editor.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertIn("surface budget", frame["fallback_reason"])
        self.assertEqual(frame["popups"], [])


if __name__ == "__main__":
    unittest.main()
