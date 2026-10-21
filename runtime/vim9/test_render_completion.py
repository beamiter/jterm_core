#!/usr/bin/env python3
"""Real Vim completion stays Vim-owned while protocol 5 paints GUI menus."""
from pathlib import Path
import tempfile
import json
import time
import unittest

from test_bridge import VIM
from test_render_delta import DeltaEditor, canonical


class CompletionEditor(DeltaEditor):
    render_protocol = 5

    def authenticate(self):
        self.send({"action": "auth", "token": self.token, "protocol": 1,
                   "render_protocol": self.render_protocol})
        return self.wait(lambda m: m.get("event") == "state")


@unittest.skipUnless(VIM, "Install Vim 9 for native completion tests")
class RenderCompletionTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-completion-")
        self.root = Path(self.directory.name)
        self.editor = CompletionEditor(self.root)
        self.configure(self.editor)

    def configure(self, e):
        e.ex("set nonumber norelativenumber nocursorline nowrap notermguicolors completeopt=menuone,noinsert,noselect pumheight=20")
        e.ex("highlight Pmenu guifg=#12ab34 guibg=#334455 ctermfg=1 ctermbg=2 gui=NONE")
        e.ex("highlight PmenuSel guifg=#f1e2d3 guibg=#a34172 ctermfg=2 ctermbg=1 gui=NONE")
        e.ex("highlight! link PmenuMatch Pmenu | highlight! link PmenuMatchSel PmenuSel")
        e.ex("highlight! link PmenuKind Pmenu | highlight! link PmenuExtra Pmenu | highlight! link PmenuKindSel PmenuSel | highlight! link PmenuExtraSel PmenuSel")
        e.ex("call setline(1, '')")
        e.gui_frame()

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def start(self, e=None):
        e = e or self.editor
        e.keys("A\x12=[complete(col('.'), [{'word':'alpha','abbr':'alpha()','kind':'f','menu':'[LSP]'}, {'word':'beta','kind':'v','menu':'[buf]'}, {'word':'界é','menu':'[utf]'}]), ''][1]\r")
        return e.painted(lambda f: f.get("completion") is not None or not f["supported"])

    def test_menu_gui_runs_unicode_and_vim_selection_acceptance(self):
        e = self.editor
        first = self.start()
        self.assertTrue(first["supported"], first.get("fallback_reason"))
        menu = first["completion"]
        self.assertEqual(menu["view"]["rows"], 3)
        self.assertEqual(menu["default_style"]["foreground"], 0x12ab34)
        self.assertTrue(any(r["text"] == "界" and r["cells"] == 2 for r in menu["runs"]))
        self.assertTrue(any(r["text"] == "é" and r["cells"] == 1 for r in menu["runs"]))
        self.assertTrue(all(r["line"] == 0 and r["byte_start"] == r["byte_end"] == 0 for r in menu["runs"]))
        self.assertTrue(all(not w["native_pointer"] for w in first["windows"]))
        e.keys("\x0e")
        selected = e.painted(lambda f: f.get("completion") is not None and any(
            r["style"].get("background") == 0xa34172 for r in f["completion"]["runs"]))
        self.assertEqual(canonical(selected), canonical(e.gui_frame()))
        e.keys("\x0e\x10\x19\x1b")
        e.painted(lambda f: f["mode"] == "n" and f.get("completion") is None)
        e.ex("update")
        self.assertEqual(e.file.read_text().splitlines()[0], "alpha")

    def test_cancel_clears_menu_in_delta_and_keeps_original_text(self):
        e = self.editor
        first = self.start()
        e.keys("\x0e\x05")
        closed = e.painted(lambda f: f["revision"] > first["revision"] and f.get("completion") is None)
        self.assertTrue(closed["supported"])
        self.assertEqual(canonical(closed), canonical(e.gui_frame()))
        e.keys("\x1b")
        e.ex("update")
        self.assertEqual(e.file.read_text().splitlines()[0], "")
        self.assertTrue(any(m["event"] == "render_delta" and m.get("completion") is None
                            and m["revision"] > first["revision"] for m in e.paint_events))

    def test_scrolling_menu_falls_back_and_close_recovers(self):
        e = self.editor
        e.ex("set pumheight=2")
        frame = self.start()
        self.assertFalse(frame["supported"])
        self.assertIn("Scrolling", frame["fallback_reason"])
        e.keys("\x05")
        recovered = e.painted(lambda f: f["supported"] and f.get("completion") is None)
        self.assertTrue(recovered["mode"].startswith("i"))

    def test_legacy_protocol_keeps_live_compatibility_menu(self):
        self.editor.close()
        class LegacyEditor(CompletionEditor):
            render_protocol = 4
        self.editor = LegacyEditor(self.root)
        self.configure(self.editor)
        frame = self.start()
        self.assertFalse(frame["supported"])
        self.assertIn("completion", frame["fallback_reason"])
        self.assertIsNone(frame.get("completion"))

    def test_custom_abbreviation_highlight_reaches_gui_menu(self):
        e = self.editor
        e.ex("highlight CompletionMark guifg=#fedcba gui=italic")
        e.ex("call timer_start(40, {-> writefile([json_encode(complete_info())], 'complete-info.json')}, {'repeat': -1})")
        e.keys("A\x12=[complete(col('.'), [{'word':'alpha', 'abbr_hlgroup':'CompletionMark'}, {'word':'beta'}]), ''][1]\r")
        frame = e.painted(lambda f: f.get("completion") is not None or not f["supported"])
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        info = None
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            e.pump(0.02)
            try:
                info = json.loads((self.root / "complete-info.json").read_text())
            except (FileNotFoundError, json.JSONDecodeError):
                continue
            if info.get("pum_visible"):
                break
        self.assertTrue(info and info.get("pum_visible"))
        if "abbr_hlgroup" not in info["items"][0]:
            self.skipTest("This Vim complete_info() omits per-item highlight metadata")
        style = next(r["style"] for r in frame["completion"]["runs"] if "alpha" in r["text"])
        self.assertEqual(style["foreground"], 0xfedcba)
        self.assertTrue(style["italic"])

    def test_cross_split_menu_is_not_buffer_source_paint(self):
        e = self.editor
        e.ex("vsplit | call setline(1, repeat('x', 52)) | normal! gg$")
        frame = self.start()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        self.assertEqual(len(frame["windows"]), 2)
        menu = frame["completion"]["view"]
        self.assertGreater(menu["column"] + menu["columns"], frame["view"]["column"] + frame["view"]["columns"])
        self.assertTrue(all(r["window"] in {w["window"] for w in frame["windows"]} for r in frame["runs"]))

    def test_custom_match_highlight_remains_explicit_compatibility(self):
        e = self.editor
        e.ex("highlight PmenuMatch guifg=#f00faa gui=bold")
        frame = self.start()
        self.assertFalse(frame["supported"])
        self.assertIn("match highlighting", frame["fallback_reason"])


if __name__ == "__main__":
    unittest.main()
