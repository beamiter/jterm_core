#!/usr/bin/env python3
"""Protocol 7 documentation hints and buffer-derived vertical scrolling."""
import unittest
from test_render_popups import PopupEditor, RenderPopupTests

class CardEditor(PopupEditor):
    render_protocol = 7

class RenderPopupCardTests(RenderPopupTests):
    def setUp(self):
        # Base setup is reused with its editor type selected locally.
        import test_render_popups
        original = test_render_popups.PopupEditor
        test_render_popups.PopupEditor = CardEditor
        try:
            super().setUp()
        finally:
            test_render_popups.PopupEditor = original

    def test_card_capability_kind_and_no_source_anchors(self):
        self.assertTrue(self.read_json('g:jterm_vim9_popup_cards'))
        options = {'line': 3, 'col': 5, 'padding': [1,1,1,1], 'wrap': 0}
        frame = self.create(['hover text', '中文文档'], options,
            after="call setwinvar(g:p, 'jterm_native_popup_kind', 'hover')")
        self.assertEqual(self.popup(frame)['kind'], 'hover')
        for kind in ['unknown', 42]:
            self.editor.ex("call setwinvar(g:p, 'jterm_native_popup_kind', " + repr(kind) + ")")
            self.assertEqual(self.popup(self.editor.gui_frame())['kind'], '')
        self.editor.ex("call setwinvar(g:p, 'jterm_native_popup_kind', 'hover') | call popup_setoptions(g:p, {'border': [1,1,1,1]})")
        self.assertEqual(self.popup(self.editor.gui_frame())['kind'], '')

    def test_protocol_six_does_not_advertise_cards(self):
        self.editor.close()
        self.editor = PopupEditor(self.root)
        self.configure()
        self.assertFalse(self.read_json('g:jterm_vim9_popup_cards'))
        frame = self.create(['legacy hover'], {'padding': [1,1,1,1], 'wrap': 0},
            after="call setwinvar(g:p, 'jterm_native_popup_kind', 'hover')")
        self.assertNotIn('kind', self.popup(frame))

    def test_nowrap_scrollbar_visible_lines_and_clear(self):
        source = self.read_json('getline(1, "$")')
        self.editor.ex('highlight CardTrack guibg=#123456 | highlight CardThumb guibg=#654321')
        lines = ['row %02d 中文' % n for n in range(1,21)]
        frame = self.create(lines, {'line': 3, 'col': 5, 'padding': [1,1,1,1],
            'wrap': 0, 'maxheight': 4, 'scrollbar': 1, 'firstline': 8,
            'scrollbarhighlight': 'CardTrack', 'thumbhighlight': 'CardThumb'},
            after="call setwinvar(g:p, 'jterm_native_popup_kind', 'diagnostic')")
        popup = self.popup(frame)
        self.assertEqual(popup['kind'], 'diagnostic')
        pos = self.read_json('popup_getpos(g:p)')
        text = ''.join(r['text'] for r in popup['runs'])
        self.assertIn('row 08', text)
        self.assertIn('row 11', text)
        self.assertNotIn('row 07', text)
        self.assertNotIn('row 12', text)
        self.assertEqual(self.read_json('getline(1, "$")'), source)
        self.assertEqual(pos['firstline'], 8)
        self.assertEqual(popup['view']['columns'], pos['width'] + 1)
        bar = popup['runs'][-pos['core_height']:]
        self.assertEqual([r['style']['background'] for r in bar],
                         [0x123456, 0x654321, 0x123456, 0x123456])
        self.editor.ex("call popup_setoptions(g:p, {'firstline': 18})")
        bottom = self.popup(self.editor.gui_frame())
        self.assertIn('row 18', ''.join(r['text'] for r in bottom['runs']))
        self.editor.ex('call popup_close(g:p)')
        self.assertEqual(self.editor.gui_frame()['popups'], [])

# Existing protocol-6 tests are maintained in test_render_popups.py; only the
# new cases run here, because some intentionally exercise protocol-6 limits.
for name in list(vars(RenderPopupTests)):
    if name.startswith('test_'):
        setattr(RenderPopupCardTests, name, None)

del RenderPopupTests

if __name__ == '__main__':
    unittest.main()
