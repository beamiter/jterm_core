#!/usr/bin/env python3
"""Post-command native paint with reconciliation polling explicitly disabled.

Uses a real Vim and PTY. Set VIM9_THEME_RUNTIME to replay the same assertions
against an older bridge. No forced snapshots are requested after measured input.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import tempfile
import time
import unittest

import test_bridge
from test_bridge import VIM
from test_render_delta import DeltaEditor

if os.environ.get('VIM9_THEME_RUNTIME'):
    test_bridge.RUNTIME = Path(os.environ['VIM9_THEME_RUNTIME']).resolve()


@unittest.skipUnless(VIM, 'Install Vim 9 for post-command paint tests')
class PostCommandThemeTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='vim9-theme-timing-')
        self.root = Path(self.directory.name)
        self.editor = DeltaEditor(self.root)
        e = self.editor
        e.ex('syntax on | set nonumber norelativenumber notermguicolors')
        e.ex('highlight Normal guifg=#ddeeff guibg=#101820 ctermfg=15 ctermbg=1')
        e.ex('highlight NativeWake guifg=#19c3a7 guibg=#112233 ctermfg=196 ctermbg=21')
        e.ex('syntax match NativeWake /^original/')
        self.original = e.file.read_bytes()
        self.initial = e.gui_frame()
        self.assertTrue(self.initial['supported'])
        self.assertTrue(self.has_foreground(self.initial, 0x19c3a7))
        # Stop only the shipped low-rate Poll callback; render/metadata timers
        # remain real. A passing test cannot depend on the 750ms reconciliation.
        e.ex("let g:reconciliation = filter(timer_info(), {_, t -> string(t.callback) =~# '_Poll'}) | "
             "call writefile([string(len(g:reconciliation))], 'poll-count') | "
             "for t in g:reconciliation | call timer_stop(t.id) | endfor")
        self.assertEqual((self.root / 'poll-count').read_text().strip(), '1')

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    @staticmethod
    def has_foreground(frame, rgb):
        return frame['supported'] and any(run['text'] == 'original'
            and run['style']['foreground'] == rgb for run in frame['runs'])

    def observe(self, command, predicate, *, cancel=False, batched=False):
        e = self.editor
        revision = e.paint['revision']
        if batched:
            started = time.monotonic()
            e.keys(':' + command + '\r')
        else:
            e.keys(':' + command)
            e.painted(lambda f: f['revision'] > revision and f['mode'].startswith('c'))
            started = time.monotonic()
            # Ctrl-C cancels Ex without the PTY's ambiguous Escape-key timeout.
            e.keys('\x03' if cancel else '\r')
        frame = e.painted(lambda f: f['revision'] > revision and f['mode'] == 'n'
                          and predicate(f), timeout=1.0)
        elapsed = (time.monotonic() - started) * 1000
        self.assertEqual(e.file.read_bytes(), self.original)
        self.assertEqual(frame['window'], self.initial['window'])
        self.assertEqual(frame['buffer'], self.initial['buffer'])
        self.assertEqual(frame['changedtick'], self.initial['changedtick'])
        print(json.dumps({'test': self.id(), 'post_command_paint_ms': elapsed,
                          'poll_disabled': True, 'source_unchanged': True}), flush=True)
        return frame

    def test_highlight_changes_publish_without_poll(self):
        self.observe('highlight NativeWake guifg=#c34983',
                     lambda f: self.has_foreground(f, 0xc34983))

    def test_link_retarget_reads_post_command_definition(self):
        self.editor.ex('highlight NewWake guifg=#c34983 ctermfg=196')
        self.observe('highlight! link NativeWake NewWake',
                     lambda f: self.has_foreground(f, 0xc34983))

    def test_normal_background_changes_without_source_edit(self):
        self.observe('highlight Normal guibg=#213345',
                     lambda f: f['supported'] and f['default_style']['background'] == 0x213345)

    def test_cancelled_command_does_not_apply_unexecuted_theme(self):
        self.observe('highlight NativeWake guifg=#c34983',
                     lambda f: self.has_foreground(f, 0x19c3a7), cancel=True)

    def test_same_batch_command_does_not_require_a_cmdline_paint(self):
        self.observe('highlight NativeWake guifg=#c34983',
                     lambda f: self.has_foreground(f, 0xc34983), batched=True)

    def test_repeated_theme_changes_do_not_reuse_old_style(self):
        for value in (0xc34983, 0x19c3a7, 0x67abde):
            self.observe(f'highlight NativeWake guifg=#{value:06x}',
                         lambda f: self.has_foreground(f, value))


if __name__ == '__main__':
    unittest.main()
