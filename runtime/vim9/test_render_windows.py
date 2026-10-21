#!/usr/bin/env python3
"""Real Vim PTY checks for semantic rendering across Vim-owned split windows.

Run beside test_bridge.py with: python3 -m unittest test_render_windows
The tests intentionally use real input, mappings, buffers and saves. They
also assert that read-only GUI sampling cannot activate another window.
"""

from __future__ import annotations

import json
from pathlib import Path
import tempfile
import time
import unittest

from test_bridge import Editor, VIM


@unittest.skipUnless(VIM, "Install Vim 9 to exercise the real editor")
class RenderWindowTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="jterm-vim9-windows-")
        self.root = Path(self.directory.name)
        self.editor = Editor(self.root)

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def assert_native(self):
        frame = self.editor.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        return frame

    def wait_file(self, name: str):
        path = self.root / name
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            try:
                return json.loads(path.read_text(encoding="utf-8"))
            except (FileNotFoundError, json.JSONDecodeError):
                pass
            self.editor.pump(0.02)
        self.fail(f"Vim did not finish writing {name}")

    def test_overlay_anchors_keep_tabs_and_cursor_source_bytes(self):
        e = self.editor
        e.ex("set tabstop=4 nonumber norelativenumber")
        e.ex("call setline(1, ['a\tbc界é', ''])")
        e.ex("normal! gg0")
        frame = self.assert_native()
        runs = [r for r in frame["runs"] if r["line"] == 1 and r["byte_end"] > r["byte_start"]]
        tab = next(r for r in runs if r["byte_start"] == 1)
        self.assertEqual((tab["text"], tab["cells"], tab["byte_end"]), ("   ", 3, 2))
        following = next(r for r in runs if r["byte_start"] == 2)
        self.assertEqual((following["text"], following["cells"], following["byte_end"]), ("bc", 2, 4))
        wide = next(r for r in runs if r["text"] == "界")
        combining = next(r for r in runs if r["text"] == "é")
        self.assertEqual((wide["byte_start"], wide["byte_end"], wide["cells"]), (4, 7, 2))
        self.assertEqual((combining["byte_start"], combining["byte_end"], combining["cells"]), (7, 10, 1))
        self.assertEqual((frame["cursor"]["line"], frame["cursor"]["byte_column"]), (1, 0))
        e.keys("A")
        e.state_where(lambda s: s["mode"] == "i")
        end = self.assert_native()["cursor"]
        self.assertEqual((end["line"], end["byte_column"]), (1, 10))
        e.keys(b"\x1b")
        e.ex("normal! G0")
        blank = self.assert_native()["cursor"]
        self.assertTrue(blank["visible"])
        self.assertEqual((blank["line"], blank["byte_column"]), (2, 0))

    def test_split_buffers_edit_save_and_gui_syntax_are_vim_authoritative(self):
        e = self.editor
        e.ex("syntax match GuiWord /original/")
        e.ex("highlight GuiWord guifg=#12ab34 ctermfg=1 gui=italic")
        first = self.assert_native()
        original_window = first["window"]
        e.request("split", vertical=True)
        e.request("open", path=str(e.other))
        e.state_where(lambda s: s["file"] == str(e.other))
        e.ex("syntax match GuiPeer /other/")
        e.ex("highlight GuiPeer guifg=#ab3412 ctermfg=4 gui=bold")
        split = self.assert_native()
        self.assertEqual(len(split["windows"]), 2)
        self.assertEqual(len({w["buffer"] for w in split["windows"]}), 2)
        self.assertEqual(sum(w["cursor"]["visible"] for w in split["windows"]), 1)
        peer_window = split["window"]
        self.assertNotEqual(peer_window, original_window)
        paint = {(r["window"], r["text"]): r["style"] for r in split["runs"]}
        self.assertEqual(paint[original_window, "original"]["foreground"], 0x12ab34)
        self.assertEqual(paint[peer_window, "other"]["foreground"], 0xab3412)
        self.assertTrue(paint[peer_window, "other"]["bold"])
        e.keys("i中文e\u0301😀 ")
        e.state_where(lambda s: s["mode"] == "i" and s["modified"])
        inserted = self.assert_native()
        self.assertEqual(inserted["window"], peer_window)
        self.assertTrue(any(r["text"] == "中" for r in inserted["runs"] if r["window"] == peer_window))
        self.assertFalse(any("中" in r["text"] for r in inserted["runs"] if r["window"] == original_window))
        e.request("save")
        self.assertTrue(e.other.read_text(encoding="utf-8").startswith("中文e\u0301😀 other"))
        self.assertEqual(e.file.read_text(encoding="utf-8"), "original line\nsecond line\n")
        e.keys(b"\x1b")
        e.state_where(lambda s: s["mode"] == "n")
        e.request("split", vertical=False)
        three = self.assert_native()
        self.assertEqual(len(three["windows"]), 3)
        windows = {w["window"]: w for w in three["windows"]}
        for run in three["runs"]:
            view = windows[run["window"]]["view"]
            self.assertLessEqual(view["row"], run["row"])
            self.assertLess(run["row"], view["row"] + view["rows"])
            self.assertLessEqual(view["column"], run["column"])
            self.assertLessEqual(run["column"] + run["cells"], view["column"] + view["columns"])

    def test_same_buffer_windows_keep_independent_scroll_and_unicode_geometry(self):
        e = self.editor
        e.ex("call setline(1, map(range(1, 200), {_, n -> printf('%03d 界é\\tvalue', n)}))")
        e.ex("set tabstop=4 number")
        first = self.assert_native()
        original_window = first["window"]
        e.request("split", vertical=True)
        e.request("goto", line=100, column=5)
        e.keys("zt")
        split = self.assert_native()
        windows = {w["window"]: w for w in split["windows"]}
        self.assertEqual(len(windows), 2)
        self.assertEqual(len({w["buffer"] for w in windows.values()}), 1)
        active = windows[split["window"]]
        inactive = windows[original_window]
        self.assertGreaterEqual(active["topline"], 90)
        self.assertEqual(inactive["topline"], 1)
        self.assertTrue(any(r["line"] == 100 for r in split["runs"] if r["window"] == split["window"]))
        self.assertFalse(any(r["line"] == 100 for r in split["runs"] if r["window"] == original_window))
        for window in windows.values():
            wide = next(r for r in split["runs"] if r["window"] == window["window"] and r["text"] == "界")
            combining = next(r for r in split["runs"] if r["window"] == window["window"] and r["text"] == "é")
            self.assertEqual((wide["byte_start"], wide["byte_end"], wide["cells"]), (4, 7, 2))
            self.assertEqual((combining["byte_start"], combining["byte_end"], combining["cells"]), (7, 10, 1))
            self.assertEqual(combining["column"], wide["column"] + 2)
        self.assertFalse(inactive["cursor"]["visible"])
        # Overlapping scrolls reuse cached semantic lines but must move every
        # glyph to its new absolute row and leave the other viewport untouched.
        for _ in range(4):
            e.keys(b"\x05")
            moved = self.assert_native()
            moved_windows = {w["window"]: w for w in moved["windows"]}
            self.assertEqual(moved_windows[original_window]["topline"], 1)
            for run in moved["runs"]:
                if run["line"]:
                    window = moved_windows[run["window"]]
                    self.assertEqual(run["row"], window["view"]["row"] + run["line"] - window["topline"])

    def test_visual_sampling_preserves_focus_view_registers_and_autocommands(self):
        e = self.editor
        e.request("split", vertical=True)
        e.ex("highlight Visual guifg=#ffffff guibg=#654321 gui=NONE")
        # <Cmd> mappings record state without leaving Visual or Insert mode.
        probe = self.root / "read-state.vim"
        probe.write_text(
            "let g:sample_events = []\n"
            "augroup SampleAudit\n"
            "  autocmd!\n"
            "  autocmd WinEnter,WinLeave,BufEnter,BufLeave * call add(g:sample_events, 1)\n"
            "augroup END\n"
            "function! SampleState(name)\n"
            "  let state = {'window':win_getid(), 'buffer':bufnr(), 'mode':mode(1), 'view':winsaveview(), 'cursor':getpos('.'), 'anchor':getpos('v'), 'jumps':getjumplist(), 'register':getreginfo('\"'), 'cwd':getcwd(), 'events':copy(g:sample_events)}\n"
            "  call writefile([json_encode(state)], a:name)\n"
            "endfunction\n"
            "nnoremap <F11> <Cmd>call SampleState('before.json')<CR>\n"
            "vnoremap <F11> <Cmd>call SampleState('before.json')<CR>\n"
            "nnoremap <F12> <Cmd>call SampleState('after.json')<CR>\n"
            "vnoremap <F12> <Cmd>call SampleState('after.json')<CR>\n",
            encoding="utf-8",
        )
        e.ex("source " + str(probe))
        e.keys("vll")
        e.state_where(lambda s: s["mode"] == "v")
        e.keys(b"\x1b[23~")
        before = self.wait_file("before.json")
        frame = self.assert_native()
        self.assertEqual(frame["mode"], "v")
        selected = [r for r in frame["runs"] if r["style"]["background"] == 0x654321]
        self.assertTrue(selected)
        self.assertEqual({r["window"] for r in selected}, {frame["window"]})
        e.keys(b"\x1b[24~")
        after = self.wait_file("after.json")
        self.assertEqual(before, after)
        self.assertEqual(after["events"], [])

    def test_unsupported_peer_clears_whole_frame_and_recovers(self):
        e = self.editor
        e.request("split", vertical=True)
        self.assertEqual(len(self.assert_native()["windows"]), 2)
        e.ex("call win_execute(win_getid(2), 'setlocal list')")
        unsupported = e.gui_frame()
        self.assertFalse(unsupported["supported"])
        self.assertIn("Window", unsupported["fallback_reason"])
        self.assertEqual(unsupported["runs"], [])
        self.assertEqual(unsupported["windows"], [])
        e.ex("call win_execute(win_getid(2), 'setlocal nolist')")
        self.assertEqual(len(self.assert_native()["windows"]), 2)
        e.ex("only")
        self.assertEqual(len(self.assert_native()["windows"]), 1)

    def test_syntax_command_invalidation_updates_unchanged_buffer_and_peer(self):
        e = self.editor
        e.ex("syntax match GuiCache /original/")
        e.ex("highlight GuiCache guifg=#12ab34")
        e.request("split", vertical=True)
        before = self.assert_native()
        self.assertEqual({r["text"] for r in before["runs"] if r["style"]["foreground"] == 0x12ab34}, {"original"})
        e.ex("syntax clear GuiCache | syntax match GuiCache /second/")
        after = self.assert_native()
        self.assertEqual(after["changedtick"], before["changedtick"])
        self.assertEqual(len(after["windows"]), 2)
        self.assertEqual({r["text"] for r in after["runs"] if r["style"]["foreground"] == 0x12ab34}, {"second"})

    def test_async_plugin_has_explicit_syntax_invalidation_hook(self):
        e = self.editor
        e.ex("syntax match GuiCache /original/")
        e.ex("highlight GuiCache guifg=#12ab34")
        plugin = self.root / "async-syntax.vim"
        plugin.write_text(
            "function! ChangeSyntax(timer)\n"
            "  syntax clear GuiCache\n"
            "  syntax match GuiCache /second/\n"
            "  doautocmd <nomodeline> User JTermVim9RenderInvalidate\n"
            "  call writefile(['true'], 'plugin-done.json')\n"
            "endfunction\n"
            "nnoremap <F10> <Cmd>call timer_start(100, 'ChangeSyntax')<CR>\n",
            encoding="utf-8",
        )
        e.ex("source " + str(plugin))
        before = self.assert_native()
        e.keys(b"\x1b[21~")
        self.wait_file("plugin-done.json")
        after = self.assert_native()
        self.assertEqual(after["changedtick"], before["changedtick"])
        self.assertEqual({r["text"] for r in after["runs"] if r["style"]["foreground"] == 0x12ab34}, {"second"})

    def test_edit_invalidates_cached_multiline_syntax_in_every_window(self):
        e = self.editor
        e.ex("call setline(1, ['START', 'inside one', 'inside two', 'END', 'outside'])")
        e.ex("syntax region GuiRegion start=/START/ end=/END/")
        e.ex("syntax sync fromstart")
        e.ex("highlight GuiRegion guifg=#12ab34")
        e.request("save")
        e.request("split", vertical=True)
        before = self.assert_native()
        self.assertEqual(len({r["window"] for r in before["runs"] if r["line"] == 3 and r["style"]["foreground"] == 0x12ab34}), 2)
        # Real Normal-mode editing, no command-line transition or provider
        # hook: changedtick alone must invalidate both windows' syntax caches.
        e.keys("gg0D")
        e.state_where(lambda s: s["mode"] == "n" and s["modified"])
        after = self.assert_native()
        self.assertGreater(after["changedtick"], before["changedtick"])
        self.assertFalse(any(r["line"] == 3 and r["style"]["foreground"] == 0x12ab34 for r in after["runs"]))
        self.assertEqual({r["window"] for r in after["runs"] if r["line"] == 3}, {w["window"] for w in after["windows"]})


if __name__ == "__main__":
    unittest.main()
