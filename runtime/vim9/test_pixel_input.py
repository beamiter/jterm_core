#!/usr/bin/env python3
"""Real Vim generation-checked precision scroll and source-pointer gestures."""
import tempfile
import unittest
from pathlib import Path
from test_bridge import Editor, VIM


@unittest.skipUnless(VIM, 'Install Vim 9 for native pointer/scroll tests')
class PixelInputTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='vim9-pixel-')
        self.editor = Editor(Path(self.directory.name), authenticate=False)
        e = self.editor
        e.send({'action': 'auth', 'token': e.token, 'protocol': 1, 'render_protocol': 3})
        e.wait(lambda m: m.get('event') == 'state')
        e.ex("call setline(1, map(range(1, 160), {_, n -> printf('%03d 界 é', n)}))")
        e.ex('set nowrap scrolloff=0 mouse=a')
        e.ex('normal! 30Gzt')

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def window(self):
        frame = self.editor.gui_frame()
        self.assertTrue(frame['supported'], frame.get('fallback_reason'))
        return next(w for w in frame['windows'] if w['window'] == frame['window'])

    def source(self, window=None):
        w = self.window() if window is None else window
        return {key: w[key] for key in ('window', 'buffer', 'changedtick')}

    def pointer(self, phase, line, byte=0, gesture=1, ok=True, source=None):
        return self.editor.request('pointer', **(self.source() if source is None else source),
                                   phase=phase, gesture=gesture, line=line, byte_column=byte, ok=ok)

    def test_scroll_acknowledges_exact_request_and_keeps_focus(self):
        w = self.window()
        request = self.editor.request('scroll_view', **self.source(w), topline=w['topline'],
                                      leftcol=w['leftcol'], lines=1)
        after = self.window()
        self.assertEqual(after['topline'], w['topline'] + 1)
        self.assertEqual(after['scroll_ack'], request['id'])
        self.assertEqual(after['window'], w['window'])
        self.assertEqual(after['changedtick'], w['changedtick'])

    def test_stale_scroll_does_not_move_a_repurposed_viewport(self):
        w = self.window()
        self.editor.ex('normal! 50Gzt')
        self.editor.request('scroll_view', **self.source(w), topline=w['topline'],
                            leftcol=w['leftcol'], lines=1, ok=False)
        self.assertEqual(self.window()['topline'], 50)

    def test_click_uses_source_bytes_even_after_a_scroll_acknowledgement(self):
        w = self.window()
        source = self.source(w)
        self.editor.request('scroll_view', **source, topline=w['topline'], leftcol=w['leftcol'], lines=2)
        self.pointer('press', 31, 4, source=source)
        self.pointer('release', 31, 4, source=source)
        state = self.editor.snapshot()
        self.assertEqual((state['line'], state['column'], state['mode']), (31, 5, 'n'))
        self.assertEqual(state['changedtick'], source['changedtick'])

    def test_drag_is_real_vim_visual_selection_and_yank_keeps_unicode(self):
        source = self.source()
        self.pointer('press', 31, 0, source=source)
        self.pointer('drag', 31, 4, source=source)
        self.pointer('release', 31, 4, source=source)
        self.assertEqual(self.editor.snapshot()['mode'], 'v')
        self.editor.keys('y')
        self.editor.state_where(lambda s: s['mode'] == 'n')
        self.editor.ex("call writefile([getreg('\"')], 'pixel-yank.txt')")
        self.assertEqual((Path(self.directory.name) / 'pixel-yank.txt').read_text().strip(), '031 界')

    def test_edits_and_invalid_utf8_boundaries_fail_without_cursor_changes(self):
        source = self.source()
        self.pointer('press', 31, 5, source=source, ok=False)
        before = self.editor.snapshot()
        self.assertEqual(before['line'], 30)
        self.editor.ex("call setline(31, 'replacement')")
        self.pointer('press', 31, 0, source=source, ok=False)
        self.assertEqual(self.editor.snapshot()['line'], 30)

    def test_keyboard_interrupt_prevents_a_late_drag_from_reentering_visual(self):
        source = self.source()
        self.pointer('press', 31, 0, source=source)
        self.pointer('drag', 31, 4, source=source)
        self.editor.keys(b'\x1b')
        self.editor.state_where(lambda s: s['mode'] == 'n')
        self.pointer('drag', 32, 0, source=source, ok=False)
        self.assertEqual(self.editor.snapshot()['mode'], 'n')

    def test_custom_mappings_and_select_mode_keep_compatibility_ownership(self):
        self.assertTrue(self.window()['native_pointer'])
        self.editor.ex('nnoremap <LeftMouse> <Nop>')
        self.assertFalse(self.window()['native_pointer'])
        self.pointer('press', 31, 0, ok=False)
        self.editor.ex('nunmap <LeftMouse>')
        self.editor.ex('set selectmode=mouse')
        self.assertFalse(self.window()['native_pointer'])

    def test_visible_empty_lines_and_eol_have_explicit_byte_lengths(self):
        self.editor.ex("call setline(31, '')")
        w = self.window()
        self.assertEqual(w['line_lengths'][1], 0)
        self.assertEqual(len(w['line_lengths']), w['botline'] - w['topline'] + 1)
        self.pointer('press', 31)
        self.pointer('release', 31)
        self.assertEqual(self.editor.snapshot()['column'], 1)

    def test_drag_cannot_cross_into_another_split(self):
        self.editor.ex('vsplit')
        frame = self.editor.gui_frame()
        current = next(w for w in frame['windows'] if w['window'] == frame['window'])
        other = next(w for w in frame['windows'] if w['window'] != frame['window'])
        self.pointer('press', 31, source=self.source(current))
        self.pointer('drag', 32, source=self.source(other), ok=False)
        self.assertEqual(self.editor.gui_frame()['window'], current['window'])


if __name__ == '__main__':
    unittest.main()
