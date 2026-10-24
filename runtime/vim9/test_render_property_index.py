#!/usr/bin/env python3
"""Real Vim checks for frame-local diagnostic property indexing.

The optional VIM9_PROPERTY_BASELINE names a previous bridge for a full semantic
frame differential replay. No Vim evaluator, property API or paint data is mocked.
"""
from __future__ import annotations

import os
from pathlib import Path
import tempfile
import unittest

import test_bridge
from test_bridge import Editor, VIM
from test_render_delta import canonical


def configure(editor):
    editor.ex("set nonumber norelativenumber nocursorline nocursorcolumn nowrap nolist")
    editor.ex("highlight Normal guifg=#eeeeee guibg=#20242c gui=NONE")
    editor.ex("highlight PropRed guifg=#de1234 guisp=#de1234 gui=undercurl")
    editor.ex("highlight PropGreen guifg=#12de34 guisp=#12de34 gui=bold")
    editor.ex("call setline(1, map(range(1,300), {_,n -> printf('line_%03d abcdefghijklmnopqrstuvwxyz 中文é😀', n)}))")
    editor.ex("call prop_type_add('GlobalPaint', {'highlight':'PropRed', 'priority':10})")


def foreground(frame, line, byte=0, window=None):
    window = frame["window"] if window is None else window
    return next(run["style"]["foreground"] for run in frame["runs"]
                if run["window"] == window and run["line"] == line
                and run["byte_start"] <= byte < run["byte_end"])


@unittest.skipUnless(VIM, "Install Vim 9 for native property tests")
class PropertyIndexTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-property-index-")
        self.root = Path(self.directory.name)
        self.editor = Editor(self.root)
        configure(self.editor)

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def native(self):
        frame = self.editor.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        return frame

    def test_same_named_global_and_buffer_local_types_remain_distinct(self):
        e = self.editor
        e.ex("call prop_add(1,1,{'type':'GlobalPaint','length':4})")
        e.ex("call prop_type_add('GlobalPaint', {'bufnr':bufnr(), 'highlight':'PropGreen', 'priority':20})")
        e.ex("call prop_add(2,1,{'type':'GlobalPaint','length':4})")
        frame = self.native()
        self.assertEqual(foreground(frame, 1), 0xde1234)
        self.assertEqual(foreground(frame, 2), 0x12de34)

    def test_type_change_refreshes_cached_lines_without_source_edit(self):
        e = self.editor
        e.ex("call prop_add(1,1,{'type':'GlobalPaint','length':4})")
        before = self.native()
        self.assertEqual(foreground(before, 1), 0xde1234)
        e.ex("call prop_type_change('GlobalPaint', {'highlight':'PropGreen'})")
        after = self.native()
        self.assertEqual(before['changedtick'], after['changedtick'])
        self.assertEqual(foreground(after, 1), 0x12de34)
        e.ex("call prop_type_delete('GlobalPaint')")
        removed = self.native()
        self.assertEqual(before['changedtick'], removed['changedtick'])
        self.assertEqual(foreground(removed, 1), 0xeeeeee)

    def test_removing_and_relocating_properties_cannot_reuse_stale_paint(self):
        e = self.editor
        e.ex("call prop_add(1,1,{'type':'GlobalPaint','length':4,'id':21})")
        before = self.native()
        e.ex("call prop_remove({'id':21,'all':1}) | call prop_add(2,1,{'type':'GlobalPaint','length':4,'id':22})")
        after = self.native()
        self.assertEqual(before['changedtick'], after['changedtick'])
        self.assertEqual(foreground(after, 1), 0xeeeeee)
        self.assertEqual(foreground(after, 2), 0xde1234)
        e.ex("call prop_remove({'id':22,'all':1})")
        self.assertEqual(foreground(self.native(), 2), 0xeeeeee)

    def test_multiline_ranges_and_priority_survive_line_bucketing(self):
        e = self.editor
        e.ex("call prop_add(1,1,{'type':'GlobalPaint','end_lnum':3,'end_col':5})")
        e.ex("call prop_type_add('PriorityPaint', {'highlight':'PropGreen','priority':30})")
        e.ex("call prop_add(2,1,{'type':'PriorityPaint','length':4})")
        frame = self.native()
        self.assertEqual(foreground(frame, 1), 0xde1234)
        self.assertEqual(foreground(frame, 2), 0x12de34)
        self.assertEqual(foreground(frame, 3), 0xde1234)
        self.assertEqual(foreground(frame, 3, 6), 0xeeeeee)
        e.ex("call prop_type_change('PriorityPaint', {'priority':1})")
        self.assertEqual(foreground(self.native(), 2), 0xde1234)

    def test_frame_and_per_line_property_budgets_still_fail_closed(self):
        e = self.editor
        script = self.root / 'budget.vim'
        script.write_text("for n in range(1,129)\n call prop_add(1,1,{'type':'GlobalPaint','length':1,'id':n})\nendfor\n")
        e.ex('source ' + str(script))
        frame = e.gui_frame()
        self.assertFalse(frame['supported'])
        self.assertIn('line exceeds the native property paint budget', frame['fallback_reason'])
        e.ex('call prop_clear(1,300)')
        self.native()
        script.write_text("for row in range(1,20)\n for n in range(1,110)\n call prop_add(row,1,{'type':'GlobalPaint','length':1,'id':n})\n endfor\nendfor\n")
        e.ex('source ' + str(script))
        frame = e.gui_frame()
        self.assertFalse(frame['supported'])
        self.assertIn('property list exceeds', frame['fallback_reason'])
        e.ex('call prop_clear(1,300)')
        self.native()

    def test_split_properties_have_independent_buffer_local_definitions(self):
        e = self.editor
        e.ex("call prop_type_add('LocalPaint', {'bufnr':bufnr(), 'highlight':'PropRed'})")
        e.ex("call prop_add(1,1,{'type':'LocalPaint','length':4})")
        first = self.native()
        first_window = first['window']
        e.request('split', vertical=True)
        e.request('open', path=str(e.other))
        e.ex("call prop_type_add('LocalPaint', {'bufnr':bufnr(), 'highlight':'PropGreen'})")
        e.ex("call prop_add(1,1,{'type':'LocalPaint','length':4})")
        frame = self.native()
        self.assertEqual(len(frame['windows']), 2)
        self.assertEqual(foreground(frame, 1, window=first_window), 0xde1234)
        self.assertEqual(foreground(frame, 1), 0x12de34)


@unittest.skipUnless(VIM and os.environ.get('VIM9_PROPERTY_BASELINE'),
                     'Set VIM9_PROPERTY_BASELINE for the controlled differential replay')
class PropertyDifferentialTests(unittest.TestCase):
    def test_full_frames_match_before_and_after_indexing(self):
        commands = [
            "call prop_add(1,1,{'type':'GlobalPaint','end_lnum':3,'end_col':5})",
            "call prop_type_add('HighPaint', {'highlight':'PropGreen','priority':20}) | call prop_add(2,2,{'type':'HighPaint','length':8})",
            "call prop_type_change('GlobalPaint', {'priority':50})",
            "call prop_type_change('GlobalPaint', {'highlight':'PropGreen','combine':0})",
            "call prop_remove({'type':'HighPaint','all':1})",
            "call prop_clear(1,300)",
            "call prop_add(2,1,{'type':'GlobalPaint','length':4}) | normal! 2Gzt",
            "normal! gg0",
            "vsplit",
            "normal! 10Gzt",
            "only | normal! gg0",
        ]
        current = test_bridge.RUNTIME
        snapshots = []
        try:
            for runtime in (Path(os.environ['VIM9_PROPERTY_BASELINE']), current):
                test_bridge.RUNTIME = runtime
                with tempfile.TemporaryDirectory(prefix='vim9-prop-diff-') as directory:
                    editor = Editor(Path(directory))
                    try:
                        configure(editor)
                        frames = []
                        for command in commands:
                            editor.ex(command)
                            frame = editor.gui_frame()
                            self.assertTrue(frame['supported'], frame.get('fallback_reason'))
                            frames.append(canonical(frame))
                        snapshots.append(frames)
                    finally:
                        editor.close()
        finally:
            test_bridge.RUNTIME = current
        for command, before, after in zip(commands, *snapshots):
            with self.subTest(command=command):
                self.assertEqual(before, after)


if __name__ == '__main__':
    unittest.main()
