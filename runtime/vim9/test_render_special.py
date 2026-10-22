#!/usr/bin/env python3
"""Read-only native help/quickfix paint with Vim-owned PTY interaction."""

import json
from pathlib import Path
import tempfile
import unittest

from test_bridge import VIM
from test_render_wincolor import WindowColorEditor


@unittest.skipUnless(VIM, "Install Vim 9 for special-window rendering tests")
class RenderSpecialTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-special-")
        self.root = Path(self.directory.name)
        self.editor = WindowColorEditor(self.root)
        self.editor.ex("syntax on | filetype plugin on | set mouse=a ttymouse=sgr")
        self.editor.ex("highlight Normal guifg=#eeeeee guibg=#20242c ctermfg=15 ctermbg=0 gui=NONE cterm=NONE")
        self.editor.ex("highlight QuickFixLine guifg=NONE guibg=#822244 ctermfg=NONE ctermbg=1 gui=NONE cterm=NONE")
        self.editor.ex("highlight CursorLine guifg=NONE guibg=#224488 ctermfg=NONE ctermbg=2 gui=underline cterm=underline")

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def native(self):
        frame = self.editor.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        return frame

    def active(self, frame):
        return next(w for w in frame["windows"] if w["window"] == frame["window"])

    def background(self, frame, window, line, column=0):
        view = window["view"]
        row = view["row"] + line - window["topline"]
        col = view["column"] + window["textoff"] + column
        return next((r["style"]["background"] for r in reversed(frame["runs"])
                     if r["window"] == window["window"] and r["row"] == row
                     and r["column"] <= col < r["column"] + r["cells"]), frame["default_style"]["background"])

    def quickfix(self, local=False):
        items = "[{'filename':'main.txt','lnum':1,'col':2,'text':'first error'}, " \
                "{'filename':'other.txt','lnum':1,'col':1,'text':'second error'}]"
        self.editor.ex("call " + ("setloclist(0," if local else "setqflist(") + items + ") | "
                       + ("lopen" if local else "copen"))
        return self.native()

    def assert_pty_owned(self, window):
        self.assertFalse(window["native_pointer"])
        self.assertFalse(window["native_scroll"])
        self.assertTrue(window["line_lengths"])
        source = {k: window[k] for k in ("window", "buffer", "changedtick")}
        self.editor.request("pointer", **source, phase="press", gesture=1,
                            line=window["topline"], byte_column=0, ok=False)
        self.editor.request("scroll_view", **source, topline=window["topline"],
                            leftcol=window["leftcol"], lines=1, ok=False)
        self.editor.request("scroll", window=window["window"], lines=1, ok=False)

    def coordinates(self):
        self.editor.ex("call writefile([json_encode(screenpos(win_getid(),line('.'),col('.')))], 'position.json')")
        return json.loads((self.root / "position.json").read_text())

    def double_click(self):
        p = self.coordinates()
        click = f"\x1b[<0;{p['curscol']};{p['row']}M\x1b[<0;{p['curscol']};{p['row']}m"
        self.editor.keys(click)
        self.editor.pump(0.05)
        self.editor.keys(click)

    def test_quickfix_paints_real_text_and_current_entry_without_semantic_input(self):
        e = self.editor
        frame = self.quickfix()
        window = self.active(frame)
        self.assert_pty_owned(window)
        ordinary = next(w for w in frame["windows"] if w["window"] != window["window"])
        self.assertTrue(ordinary["native_pointer"])
        self.assertTrue(ordinary["native_scroll"])
        self.assertEqual(self.background(frame, window, 1), 0x822244)
        self.assertEqual(self.background(frame, window, 2), 0x20242c)
        backdrop = next(r for r in frame["runs"]
                        if r["window"] == window["window"] and r["row"] == window["view"]["row"]
                        and r["cells"] == window["view"]["columns"])
        self.assertEqual(backdrop["line"], 1,
                         "QuickFixLine must not masquerade as a whole-window Normal backdrop")
        self.assertEqual(backdrop["text"], " " * backdrop["cells"])
        self.assertEqual(backdrop["byte_start"], backdrop["byte_end"])
        e.ex("call writefile([json_encode(getline(1,'$'))], 'lines.json')")
        lines = json.loads((self.root / "lines.json").read_text())
        for index, line in enumerate(lines, 1):
            actual = "".join(r["text"] for r in frame["runs"]
                             if r["window"] == window["window"] and r["line"] == index
                             and r["byte_end"] > r["byte_start"])
            self.assertEqual(actual, line)
        e.ex("setlocal cursorline | normal! G")
        moved = self.native()
        self.assertEqual(self.background(moved, window, 1), 0x822244,
                         "The current quickfix entry is not the cursor line")
        self.assertEqual(self.background(moved, window, 2), 0x224488)
        tick = moved["changedtick"]
        e.ex("call setqflist([], 'a', {'idx':2})")
        changed = self.native()
        self.assertEqual(changed["changedtick"], tick)
        self.assertEqual(self.background(changed, window, 2), 0x822244,
                         "QuickFixLine overrides CursorLine's background")
        combined = next(r for r in changed["runs"]
                        if r["window"] == window["window"] and r["line"] == 2
                        and r["byte_start"] == r["byte_end"] and r["cells"] == window["view"]["columns"])
        self.assertTrue(combined["style"]["underline"])
        self.assertEqual(combined["text"], " " * combined["cells"],
                         "Text decoration painters need explicit EOL whitespace")

    def test_quickfix_columns_and_visual_follow_the_real_screen_priority(self):
        e = self.editor
        self.quickfix()
        e.ex("highlight ColorColumn guibg=#448822 ctermbg=2 | "
             "highlight Visual guibg=#888822 ctermbg=3 gui=NONE cterm=NONE | setlocal colorcolumn=3,8")
        command = ("redraw!<Bar>call writefile([json_encode(map(range(1,10), "
                   "{_,c -> screenattr(getwininfo(win_getid())[0].winrow,c)}))], 'oracle.json')")
        e.ex("nnoremap <F8> <Cmd>" + command + "<CR>")
        e.ex("xnoremap <F8> <Cmd>" + command + "<CR>")

        def oracle():
            path = self.root / "oracle.json"
            path.unlink(missing_ok=True)
            e.keys(b"\x1b[19~")
            for _ in range(100):
                e.pump(0.02)
                try:
                    return json.loads(path.read_text())
                except (FileNotFoundError, json.JSONDecodeError):
                    continue
            self.fail("Vim did not write the screen oracle")

        frame = self.native()
        window = self.active(frame)
        before = oracle()
        self.assertEqual(self.background(frame, window, 1, 2), 0x448822)
        self.assertNotEqual(before[0], before[2])
        e.keys("0v3l")
        e.state_where(lambda s: s["mode"] == "v")
        frame = self.native()
        after = oracle()
        self.assertEqual(self.background(frame, window, 1, 2), 0x888822)
        self.assertEqual(self.background(frame, window, 1, 7), 0x822244)
        self.assertNotEqual(after[2], before[2])
        self.assertEqual(after[7], before[0], "Visual suppresses the unselected source column guide")

    def test_long_wrapped_quickfix_text_retains_compatibility(self):
        e = self.editor
        self.quickfix()
        e.ex("call setqflist([{'filename':'main.txt','lnum':1,'text':repeat('wide message ',40)}]) | setlocal wrap")
        frame = e.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertIn("Wrapped", frame["fallback_reason"])

    def test_global_and_location_lists_keep_separate_current_entries(self):
        e = self.editor
        global_frame = self.quickfix()
        global_window = self.active(global_frame)
        e.ex("wincmd p")
        local_frame = self.quickfix(local=True)
        local_window = self.active(local_frame)
        e.ex("call setqflist([], 'a', {'idx':2})")
        frame = self.native()
        self.assertEqual(self.background(frame, global_window, 2), 0x822244)
        self.assertEqual(self.background(frame, global_window, 1), 0x20242c)
        self.assertEqual(self.background(frame, local_window, 1), 0x822244)
        self.assertEqual(self.background(frame, local_window, 2), 0x20242c)
        self.assertEqual(frame["window"], local_window["window"])

    def test_quickfix_enter_and_double_click_remain_vim_owned(self):
        e = self.editor
        self.quickfix()
        e.keys(b"\r")
        state = e.state_where(lambda s: s["file"] == str(e.file))
        self.assertEqual((state["line"], state["column"]), (1, 2))
        e.ex("copen | normal! G0")
        self.assert_pty_owned(self.active(self.native()))
        self.double_click()
        state = e.state_where(lambda s: s["file"] == str(e.other))
        self.assertEqual(state["line"], 1)
        self.assertFalse(state["modified"])

    def test_help_keeps_conceal_guard_but_plain_help_can_render(self):
        e = self.editor
        e.ex("help usr_02.txt")
        frame = e.gui_frame()
        self.assertFalse(frame["supported"])
        self.assertIn("display option", frame["fallback_reason"])
        e.ex("setlocal conceallevel=0 nowrap")
        frame = self.native()
        window = self.active(frame)
        self.assert_pty_owned(window)
        text = "".join(r["text"] for r in frame["runs"]
                       if r["window"] == window["window"] and r["line"] == 1
                       and r["byte_end"] > r["byte_start"])
        self.assertIn("*usr_02.txt*", text)
        e.ex("call writefile([string(&modifiable),string(&readonly),string(&modified)], 'options')")
        self.assertEqual((self.root / "options").read_text().splitlines(), ["0", "1", "0"])

    def test_plain_help_enter_tag_jump_and_double_click_keep_vim_semantics(self):
        e = self.editor
        e.ex("help usr_02.txt | setlocal conceallevel=0 nowrap | call search('|usr_03.txt|') | normal! l")
        self.assert_pty_owned(self.active(self.native()))
        first = e.snapshot()
        e.keys(b"\r")
        entered = e.state_where(lambda s: s["line"] != first["line"])
        self.assertEqual(entered["file"], first["file"], "Help Enter moves; it is not Ctrl-]")
        e.ex("call search('|usr_03.txt|','b') | normal! l")
        e.keys(b"\x1d")
        e.state_where(lambda s: s["file"].endswith("/usr_03.txt"))
        e.ex("help usr_02.txt | setlocal conceallevel=0 nowrap | call search('|usr_03.txt|') | normal! l")
        self.native()
        self.double_click()
        e.state_where(lambda s: s["file"].endswith("/usr_03.txt"))


if __name__ == "__main__":
    unittest.main()
