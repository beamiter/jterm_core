#!/usr/bin/env python3
"""Real SimpleFinder buffer picker, preview popup and bridge restoration."""
from __future__ import annotations

import os
from pathlib import Path
import unittest

import test_vimrc_workbench as harness

FINDER = Path(os.environ.get('VIM9_TEST_SIMPLE_FINDER',
                            harness.PLUGIN_ROOT / 'simplefinder')).resolve()
AVAILABLE = harness.AVAILABLE and (FINDER / 'plugin/simplefinder.vim').is_file()


@unittest.skipUnless(AVAILABLE, 'Needs Vim, vimrc, simple* and SimpleFinder checkouts')
class SimpleFinderTests(unittest.TestCase):
    setUp = harness.VimrcWorkbenchTests.setUp
    tearDown = harness.VimrcWorkbenchTests.tearDown
    editor = harness.VimrcWorkbenchTests.editor
    capture = harness.VimrcWorkbenchTests.capture
    workbench = harness.VimrcWorkbenchTests.workbench

    def finder_editor(self):
        e = self.editor()
        # The parent harness already isolated HOME and disabled installation.
        # Load the actual checkout explicitly; buffers needs no daemon/network.
        e.ex('let &runtimepath = ' + harness.vim_string(FINDER) + " .. ',' .. &runtimepath")
        e.ex("execute 'source ' .. fnameescape(" + harness.vim_string(FINDER / 'plugin/simplefinder.vim') + ')')
        e.ex('let g:simplefinder_preview = 1 | let g:simplefinder_debounce_ms = 0')
        e.ex("execute 'badd ' .. fnameescape(" + harness.vim_string(e.other) + ')')
        e.ex("call setline(1, 'unsaved source')")
        original = e.snapshot()
        self.assertIn('simple_buffers', original['capabilities'])
        self.assertTrue(e.gui_frame()['supported'])
        return e, original

    def open_picker(self, e):
        self.workbench(e, 'simple_buffers')
        frame = e.wait(lambda m: m.get('event') == 'render'
                       and not m.get('supported', False)
                       and 'popup' in m.get('fallback_reason', ''))
        return frame

    def assert_restored(self, e, frame):
        e.wait(lambda m: m.get('event') == 'render' and m.get('supported', False)
               and m.get('revision', 0) > frame['revision'])
        self.assertTrue(e.gui_frame()['supported'])
        self.assertEqual(self.capture(e, 'len(popup_list())'), 0)

    def test_cancel_preview_restores_source_and_keyboard_input(self):
        e, original = self.finder_editor()
        frame = self.open_picker(e)
        e.keys('missing-buffer-query')
        # Cancel the real panel even with a query that has no candidates.
        e.keys(b'\x1b')
        e.state_where(lambda s: s.get('buffer') == original['buffer'])
        self.assert_restored(e, frame)
        self.assertEqual(self.capture(e, "getline(1, '$')"), ['unsaved source', 'second line'])
        self.assertEqual(self.capture(e, '&modified'), 1)
        e.keys(b'A!\x1b')
        e.state_where(lambda s: s.get('buffer') == original['buffer']
                      and s.get('changedtick', 0) > original['changedtick'])
        self.assertEqual(self.capture(e, 'getline(1)'), 'unsaved source!')

    def test_select_buffer_with_real_query_preserves_unsaved_source(self):
        e, original = self.finder_editor()
        frame = self.open_picker(e)
        e.keys('other.txt')
        # A snapshot round trip lets the mapped query and its zero-delay
        # filtering timer complete before accepting the sole match.
        e.snapshot()
        e.keys(b'\r')
        selected = e.state_where(lambda s: s.get('buffer') != original['buffer']
                                 and s.get('file') == str(e.other))
        self.assert_restored(e, frame)
        self.assertEqual(self.capture(e, 'getline(1)'), 'other file')
        self.assertEqual(self.capture(e, f'getbufline({original["buffer"]}, 1)'), ['unsaved source'])
        self.assertEqual(self.capture(e, f'getbufvar({original["buffer"]}, "&modified")'), 1)
        e.keys(b'A!\x1b')
        e.state_where(lambda s: s.get('buffer') == selected['buffer']
                      and s.get('changedtick', 0) > selected['changedtick'])
        self.assertEqual(self.capture(e, 'getline(1)'), 'other file!')


if __name__ == '__main__':
    unittest.main(verbosity=2)
