#!/usr/bin/env python3
"""Actual vimrc/SimpleCC documentation cards with deterministic daemon replies."""

import json
import fcntl
import os
import signal
import struct
import termios
import unittest

import test_vimrc_workbench as harness
from test_render_popups import PopupEditor
import test_semantic_editing


class CardEditor(PopupEditor):
    render_protocol = 7


@unittest.skipUnless(harness.AVAILABLE, 'Needs Vim, vimrc and candidate SimpleCC')
class LspPopupCardTests(unittest.TestCase):
    setUp = harness.VimrcWorkbenchTests.setUp
    tearDown = harness.VimrcWorkbenchTests.tearDown
    editor = harness.VimrcWorkbenchTests.editor
    capture = harness.VimrcWorkbenchTests.capture
    workbench = harness.VimrcWorkbenchTests.workbench
    until = test_semantic_editing.SemanticEditingTests.until

    def requests(self):
        path = self.root / 'popup.requests'
        return [json.loads(s) for s in path.read_text().splitlines()] if path.exists() else []

    def start(self):
        self.doc = self.root / 'hover.txt'
        self.doc.write_text('Short hover documentation', encoding='utf-8')
        daemon = self.root / 'popup-daemon'
        daemon.write_text('''#!/usr/bin/env python3
from pathlib import Path
import json, sys
root = Path(__file__).parent
for line in sys.stdin:
    r = json.loads(line)
    with (root / 'popup.requests').open('a') as out:
        out.write(json.dumps(r) + '\\n')
    kind = r['type']
    event = {'id': r.get('id', 0)}
    if kind == 'initialize':
        event.update(type='initialized')
    elif kind == 'textDocument/hover':
        event.update(type='hover', contents=(root / 'hover.txt').read_text())
    elif kind == 'textDocument/signatureHelp':
        event.update(type='signatureHelp', signatures=[{
            'label': 'alpha(value: int)', 'active_parameter': 0,
            'parameters': [{'label': 'value', 'documentation': 'input value 界é'}],
            'documentation': 'Signature documentation'}])
    elif kind == 'textDocument/didOpen':
        event.update(type='diagnostics', uri=r['uri'], server='fixture', items=[{
            'line': 0, 'character': 0, 'end_line': 0, 'end_character': 6,
            'severity': 1, 'source': 'fixture', 'code': 'E1',
            'message': 'Diagnostic documentation 界é\\nUseful detail.'}])
    elif kind == 'shutdown':
        break
    else:
        continue
    print(json.dumps(event, ensure_ascii=False), flush=True)
''', encoding='utf-8')
        daemon.chmod(0o700)
        e = self.editor(editor_class=CardEditor, extra_setup=(
            'let g:simplecc_daemon_path = ' + harness.vim_string(daemon) + '\n'
            'let g:simplecc_signature_help = 0\n'
            'let g:simplecc_virtual_diag = 0\n'
            'let g:simplecc_diag_float = 0\n'
            'autocmd VimEnter * setfiletype rust | call g:VimrcConfigureFiletype() | SimpleCCStart\n'))
        self.e = e
        self.until(e, lambda: self.capture(e, "get(g:, 'simplecc_status', '')") == 'ready')
        self.source = e.snapshot()
        self.assertTrue(self.capture(e, 'g:jterm_vim9_popup_cards'))
        self.assertTrue(e.gui_frame()['supported'])
        return e

    @staticmethod
    def popup_text(popup):
        return ''.join(r['text'] for r in popup['runs'])

    def card(self, kind):
        frame = self.e.painted(lambda f: f['supported'] and any(
            p.get('kind') == kind for p in f.get('popups', [])))
        popup = next(p for p in frame['popups'] if p.get('kind') == kind)
        self.assertTrue(all(r['window'] == r['line'] == r['byte_start'] == r['byte_end'] == 0
                            for r in popup['runs']))
        return frame, popup

    def assert_source(self):
        current = self.e.snapshot()
        for key in ('window', 'buffer', 'changedtick', 'line', 'column'):
            self.assertEqual(current[key], self.source[key], key)
        self.assertEqual(self.capture(self.e, "getline(1, '$')"), ['original line', 'second line'])

    def test_long_hover_wraps_scrolls_with_vimrc_keys_and_closes_without_editing(self):
        e = self.start()
        self.doc.write_text('**Hover documentation**\n\n' + '\n'.join(
            f'row {n:02d}: ' + 'readable words 界é ' * 7 for n in range(30)), encoding='utf-8')
        e.request('hover')
        before, popup = self.card('hover')
        self.assertIn('Hover documentation', self.popup_text(popup))
        position = self.capture(e, "popup_getpos(popup_list()[0])")
        self.assertTrue(position['scrollbar'])
        self.assertEqual(self.capture(e, "popup_getoptions(popup_list()[0]).wrap"), 0)
        self.assertEqual(self.capture(e, "max(map(getbufline(winbufnr(popup_list()[0]), 1, '$'), {_,v -> strdisplaywidth(v)}))") <= position['core_width'], True)
        self.assert_source()
        e.keys(b'\x06')
        scrolled = e.painted(lambda f: f['supported'] and f['revision'] > before['revision']
                             and f.get('popups') and f['popups'][0]['runs'] != popup['runs'])
        self.assertEqual(scrolled['popups'][0]['kind'], 'hover')
        self.assertGreater(self.capture(e, "popup_getpos(popup_list()[0]).firstline"), 1)
        self.assert_source()
        e.keys(b'\x02')
        self.until(e, lambda: self.capture(e, "popup_getpos(popup_list()[0]).firstline") == 1)
        e.keys(' lq')
        e.painted(lambda f: f['supported'] and not f.get('popups'))
        self.assert_source()
        # The expr mapping must retain ordinary Vim paging without a popup.
        self.assertEqual(self.capture(e, 'simplecc#PopupScrollKey(1, "\\<C-f>")'), '\x06')

    def test_signature_and_diagnostics_use_cards_and_insert_signature_keeps_mode(self):
        e = self.start()
        e.keys(b'i\x07s')
        _, signature = self.card('signature')
        self.assertIn('alpha(value: int)', self.popup_text(signature))
        self.assertTrue(e.snapshot()['mode'].startswith('i'))
        self.assertEqual(e.snapshot()['changedtick'], self.source['changedtick'])
        e.keys(b'\x1b')
        e.state_where(lambda s: s.get('mode') == 'n')
        e.ex('SimpleCCPopupClose')
        e.ex('SimpleCCDiag')
        _, diagnostic = self.card('diagnostic')
        self.assertIn('Diagnostic documentation 界é', self.popup_text(diagnostic))
        e.keys(' lq')
        e.painted(lambda f: f['supported'] and not f.get('popups'))
        self.assert_source()

    def test_user_opt_out_keeps_classic_popup(self):
        e = self.start()
        e.ex('let g:simplecc_popup_cards = 0')
        e.request('hover')
        frame = e.painted(lambda f: f['supported'] and bool(f.get('popups')))
        self.assertFalse(frame['popups'][0].get('kind'))
        self.assertTrue(any('╭' in r['text'] for r in frame['popups'][0]['runs']))
        self.assert_source()

    def test_narrow_hover_keeps_wrapped_tab_and_unicode_text_native(self):
        e = self.start()
        fcntl.ioctl(e.master, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 48, 0, 0))
        os.kill(e.process.pid, signal.SIGWINCH)
        e.painted(lambda f: f['supported'] and f['columns'] == 48)
        e.ex('setlocal tabstop=4 vartabstop=4,8')
        self.doc.write_text('\n'.join('界é\t' + 'narrow words ' * 9 for _ in range(35)), encoding='utf-8')
        e.request('hover')
        frame, popup = self.card('hover')
        self.assertLessEqual(popup['view']['column'] + popup['view']['columns'], 48)
        self.assertIn('界', self.popup_text(popup))
        self.assertIn('é', self.popup_text(popup))
        self.assertTrue(self.capture(e, 'popup_getpos(popup_list()[0]).scrollbar'))
        self.assert_source()

    def test_completion_documentation_card_closes_on_cancel_and_accept(self):
        e = self.start()
        plugin = str((harness.PLUGIN_ROOT / 'simplecc/autoload/simplecc.vim').resolve())
        sid = self.capture(e, 'filter(getscriptinfo(), {_,s -> s.name ==# '
                           + harness.vim_string(plugin) + '})[0].sid')
        script = self.root / 'completion-doc-card.vim'
        script.write_text(
            f"let g:DocumentCard = function('<SNR>{sid}_ShowCompletionDocumentation')\n"
            "function! DocumentCardTimer(timer)\n"
            "  if !pumvisible() | return | endif\n"
            "  call g:DocumentCard({'detail':'alpha(value: int)', 'documentation':'Completion documentation 界é'})\n"
            "endfunction\n", encoding='utf-8')
        e.ex('source ' + str(script))
        e.ex('set completeopt=menuone,noinsert,noselect')
        e.ex('highlight! link PmenuMatch Pmenu | highlight! link PmenuMatchSel PmenuSel')
        for accept in (False, True):
            with self.subTest(accept=accept):
                e.keys("A\x12=[complete(col('.'), ['alpha', 'beta']), timer_start(100, 'DocumentCardTimer'), ''][2]\r")
                _, card = self.card('completion_documentation')
                self.assertIn('Completion documentation 界é', self.popup_text(card))
                e.keys(b'\x0e\x19' if accept else b'\x05')
                e.painted(lambda f: f['supported'] and f['mode'].startswith('i')
                          and f.get('completion') is None and not f.get('popups'))
                e.keys(b'\x1b')
                e.state_where(lambda s: s.get('mode') == 'n')
                expected = 'original linealpha' if accept else 'original line'
                self.assertEqual(self.capture(e, 'getline(1)'), expected)


if __name__ == '__main__':
    unittest.main(verbosity=2)
