#!/usr/bin/env python3
"""Plugin decorations against a real Vim PTY, including Vim's screen oracle."""

import json
from pathlib import Path
import tempfile
import unittest

from test_bridge import Editor, VIM


@unittest.skipUnless(VIM, "Install Vim 9 for native plugin decoration tests")
class RenderMatchTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-matches-")
        self.root = Path(self.directory.name)
        self.editor = Editor(self.root)
        self.editor.ex("set nonumber norelativenumber nocursorline notermguicolors")
        self.editor.ex("highlight Normal guifg=#d8dee9 guibg=#20242c ctermfg=15 ctermbg=0")
        self.editor.ex("highlight PluginLow guifg=#12ab34 ctermfg=2 gui=italic cterm=italic")
        self.editor.ex("highlight PluginHigh guibg=#654321 ctermbg=1 gui=NONE cterm=NONE")
        self.editor.ex("highlight Search guifg=#ffffff guibg=#246813 ctermfg=7 ctermbg=4 gui=NONE cterm=NONE")

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def native(self):
        frame = self.editor.gui_frame()
        self.assertTrue(frame["supported"], frame["fallback_reason"])
        return frame

    def style(self, frame, line, byte, window=None):
        window = frame["window"] if window is None else window
        return next(r["style"] for r in reversed(frame["runs"])
                    if r["window"] == window and r["line"] == line
                    and r["byte_start"] <= byte < r["byte_end"])

    def screen(self, count, columns):
        """Opaque screenattr IDs are compared with each other, never RGB IDs."""
        self.editor.ex("redraw! | call writefile([json_encode(map(range(1,%d), "
                       "{_,r -> map(range(1,%d), {_,c -> screenattr(r,c)})}))], 'screen.json')"
                       % (count, columns))
        return json.loads((self.root / "screen.json").read_text())

    def test_priority_selects_one_match_and_search_wins_zero_ties(self):
        e = self.editor
        e.ex("call setline(1, ['abcdef', 'reference'])")
        e.ex("let g:low = matchadd('PluginLow', 'abcd', -1)")
        e.ex("let g:high = matchadd('PluginHigh', 'bc', 10)")
        frame = self.native()
        self.assertEqual(self.style(frame, 1, 0)["foreground"], 0x12ab34)
        winner = self.style(frame, 1, 1)
        self.assertEqual(winner["background"], 0x654321)
        self.assertEqual(winner["foreground"], 0xd8dee9)
        self.assertFalse(winner["italic"], "Losing match attributes must not leak")
        attrs = self.screen(1, 6)[0]
        self.assertEqual(attrs[0], attrs[3])
        self.assertEqual(attrs[1], attrs[2])
        self.assertNotEqual(attrs[0], attrs[1])
        e.ex("let @/ = 'abcd' | set hlsearch")
        frame = self.native()
        self.assertEqual(self.style(frame, 1, 0)["background"], 0x246813)
        self.assertEqual(self.style(frame, 1, 1)["background"], 0x654321)
        e.ex("call matchdelete(g:high) | call matchadd('PluginHigh', 'abcd', 0)")
        self.assertEqual(self.style(self.native(), 1, 1)["background"], 0x246813)
        e.ex("nohlsearch")
        self.assertEqual(self.style(self.native(), 1, 1)["background"], 0x654321)

    def test_equal_priorities_follow_insertion_order_and_ignore_empty_groups(self):
        e = self.editor
        e.ex("let g:first = matchadd('PluginLow', 'original', 10)")
        e.ex("let g:last = matchadd('PluginHigh', 'original', 10)")
        e.ex("highlight EmptyPlugin NONE | call matchadd('EmptyPlugin', 'original', 100)")
        self.assertEqual(self.style(self.native(), 1, 0)["background"], 0x654321)
        e.ex("call matchdelete(g:last)")
        style = self.style(self.native(), 1, 0)
        self.assertEqual(style["foreground"], 0x12ab34)
        self.assertTrue(style["italic"])

    def test_regex_case_and_magic_are_independent_of_search_options(self):
        e = self.editor
        e.ex("call setline(1, 'word WORD wood')")
        e.ex("set ignorecase smartcase nomagic")
        e.ex("call matchadd('PluginLow', 'wo.d')")
        frame = self.native()
        for byte in (0, 10):
            self.assertEqual(self.style(frame, 1, byte)["foreground"], 0x12ab34)
        self.assertEqual(self.style(frame, 1, 5)["foreground"], 0xd8dee9)
        e.ex(r"call matchadd('PluginHigh', '\cWORD')")
        frame = self.native()
        for byte in (0, 5):
            self.assertEqual(self.style(frame, 1, byte)["background"], 0x654321)
        attrs = self.screen(1, 14)[0]
        self.assertEqual(attrs[0], attrs[5])
        self.assertNotEqual(attrs[0], attrs[10])

    def test_positional_bytes_tabs_unicode_whole_lines_and_clipping(self):
        e = self.editor
        e.ex("call setline(1, ['a\t界é end', '', 'abcdef', 'abcdef']) | set tabstop=4")
        e.ex("call matchaddpos('PluginLow', [[1,2,1], [1,3,1], [1,6,1], 2, 3, [4,5,90]])")
        frame = self.native()
        for byte in (1, 2, 5):
            self.assertEqual(self.style(frame, 1, byte)["foreground"], 0x12ab34)
        self.assertEqual(self.style(frame, 1, 8)["foreground"], 0xd8dee9)
        for byte in range(6):
            self.assertEqual(self.style(frame, 3, byte)["foreground"], 0x12ab34)
            self.assertEqual(self.style(frame, 4, byte)["foreground"],
                             0x12ab34 if byte >= 4 else 0xd8dee9)
        attrs = self.screen(4, 15)
        self.assertEqual(len(set(attrs[2][:6])), 1)
        self.assertNotEqual(attrs[2][5], attrs[2][6], "Whole-line positions stop before EOL")
        self.assertEqual(attrs[1], [attrs[2][6]] * 15, "An empty positional line paints no cell")
        self.assertNotEqual(attrs[3][3], attrs[3][4])
        e.ex("call clearmatches() | call matchaddpos('PluginLow', [[1,4,1], [1,7,1]])")
        frame = self.native()
        self.assertEqual(self.style(frame, 1, 2)["foreground"], 0xd8dee9)
        self.assertEqual(self.style(frame, 1, 5)["foreground"], 0xd8dee9)
        self.assertEqual(len(set(self.screen(1, 12)[0])), 1,
                         "A continuation byte cannot start a highlight")

    def test_match_background_overrides_cursorline_but_visual_wins_and_properties_combine(self):
        e = self.editor
        e.ex("highlight CursorLine guibg=#345678 ctermbg=3 gui=NONE cterm=NONE | set cursorline")
        e.ex("highlight Visual guibg=#abcdef ctermbg=5 gui=NONE cterm=NONE")
        e.ex("call matchaddpos('PluginHigh', [[1,1,8]])")
        self.assertEqual(self.style(self.native(), 1, 0)["background"], 0x654321)
        attrs = self.screen(1, 12)[0]
        self.assertNotEqual(attrs[0], attrs[9])
        e.keys("gg0vll")
        e.state_where(lambda s: s["mode"] == "v")
        frame = self.native()
        for byte in range(3):
            self.assertEqual(self.style(frame, 1, byte)["background"], 0xabcdef)
        self.assertEqual(self.style(frame, 1, 3)["background"], 0x654321)
        e.keys(b"\x1b")
        e.state_where(lambda s: s["mode"] == "n")
        e.ex("call prop_type_add('diagnostic', {'highlight':'PluginLow'})")
        e.ex("call prop_add(1, 1, {'type':'diagnostic', 'length':3})")
        style = self.style(self.native(), 1, 0)
        self.assertEqual(style["background"], 0x654321)
        self.assertEqual(style["foreground"], 0x12ab34)
        self.assertTrue(style["italic"])

    def test_async_plugin_updates_and_deletes_without_buffer_edit_or_hook(self):
        e = self.editor
        plugin = self.root / "plugin.vim"
        plugin.write_text(
            "function! UpdatePlugin(timer)\n"
            "  call clearmatches()\n"
            "  call matchaddpos('PluginHigh', [[2,1,6]])\n"
            "  call writefile(['done'], 'plugin-done')\n"
            "endfunction\n"
            "nnoremap <F10> <Cmd>call timer_start(100, 'UpdatePlugin')<CR>\n",
            encoding="utf-8")
        e.ex("source " + str(plugin))
        e.ex("call matchadd('PluginLow', 'original')")
        before = self.native()
        e.keys(b"\x1b[21~")
        after = e.wait(lambda m: m.get("event") == "render" and m.get("supported")
                       and any(r["line"] == 2 and r["style"]["background"] == 0x654321
                               for r in m["runs"]))
        self.assertEqual(after["changedtick"], before["changedtick"])
        self.assertEqual(self.style(after, 1, 0)["foreground"], 0xd8dee9)
        e.ex("call clearmatches()")
        after = self.native()
        self.assertEqual(after["changedtick"], before["changedtick"])
        self.assertEqual(self.style(after, 2, 0)["background"], 0x20242c)

    def test_split_matches_are_window_local_and_sampling_does_not_move_focus(self):
        e = self.editor
        e.request("split", vertical=True)
        e.ex("call clearmatches() | call matchaddpos('PluginLow', [[1,1,8]])")
        active = self.native()["window"]
        e.ex("call win_execute(win_getid(2), \"call clearmatches() | call matchaddpos('PluginHigh', [[1,1,8]])\")")
        before = e.snapshot()
        frame = self.native()
        peer = next(w["window"] for w in frame["windows"] if w["window"] != active)
        self.assertEqual(self.style(frame, 1, 0, active)["foreground"], 0x12ab34)
        self.assertEqual(self.style(frame, 1, 0, peer)["background"], 0x654321)
        after = e.snapshot()
        for key in ("file", "line", "column", "mode", "modified", "changedtick"):
            self.assertEqual(before[key], after[key])

    def test_builtin_matchparen_stays_native_and_moves_with_the_real_cursor(self):
        e = self.editor
        e.ex("runtime plugin/matchparen.vim")
        e.ex("highlight MatchParen guifg=#f1a234 guibg=#123456 ctermfg=3 ctermbg=4")
        e.ex("call setline(1, '(word) plain') | normal! gg0")
        frame = self.native()
        self.assertEqual(self.style(frame, 1, 0)["foreground"], 0xf1a234)
        self.assertEqual(self.style(frame, 1, 5)["foreground"], 0xf1a234)
        attrs = self.screen(1, 10)[0]
        self.assertEqual(attrs[0], attrs[5])
        self.assertNotEqual(attrs[0], attrs[1])
        e.keys("ll")
        e.state_where(lambda s: s["column"] == 3)
        self.assertEqual(self.style(self.native(), 1, 0)["foreground"], 0xd8dee9)

    def test_unsupported_patterns_conceal_and_budgets_fallback_then_recover(self):
        e = self.editor
        for pattern, reason in [(r"original\_sline", "Multiline"),
                                (r"\%1lword", "positional"),
                                (r"\zsoriginal", "positional"), ("^", "Zero-width")]:
            with self.subTest(pattern=pattern):
                e.ex("call matchadd('PluginLow', '" + pattern + "')")
                frame = e.gui_frame()
                self.assertFalse(frame["supported"])
                self.assertIn(reason, frame["fallback_reason"])
                self.assertEqual(frame.get("runs", []), [])
                e.ex("call clearmatches()")
                self.native()
        e.ex("call matchadd('Conceal', 'original', 10, -1, {'conceal':'x'})")
        self.assertIn("conceal", e.gui_frame()["fallback_reason"])
        e.ex("call clearmatches() | for n in range(65) | call matchadd('PluginLow', 'original') | endfor")
        self.assertIn("budget", e.gui_frame()["fallback_reason"])
        e.ex("call clearmatches() | call setline(1, repeat('x', 600)) | call matchadd('PluginLow', 'x')")
        self.assertIn("budget", e.gui_frame()["fallback_reason"])
        e.ex("call clearmatches()")
        self.native()


if __name__ == "__main__":
    unittest.main(verbosity=2)
