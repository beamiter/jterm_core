#!/usr/bin/env python3
"""Real vimrc/SimpleCC semantic actions; only daemon wire replies are fixtures.

Run with VIM9_TEST_VIMRC and VIM9_TEST_SIMPLE_ROOT as for test_vimrc_workbench.
No language server, installation, network or mocked Vim command is involved.
"""
from __future__ import annotations

import json
import time
import unittest

import test_vimrc_workbench as harness
from test_render_popups import PopupEditor
from test_vimrc_workbench import AVAILABLE, vim_string


@unittest.skipUnless(AVAILABLE, 'Needs Vim and real vimrc/simple plugin checkouts')
class SimpleNavigationTests(unittest.TestCase):
    setUp = harness.VimrcWorkbenchTests.setUp
    tearDown = harness.VimrcWorkbenchTests.tearDown
    editor = harness.VimrcWorkbenchTests.editor
    capture = harness.VimrcWorkbenchTests.capture
    workbench = harness.VimrcWorkbenchTests.workbench
    assert_pty_text = harness.VimrcWorkbenchTests.assert_pty_text

    def navigation_editor(self):
        target = self.root / 'unicode.rs'
        target.write_text('中😀target\n中😀second\n', encoding='utf-8')
        fixture = self.root / 'navigation-daemon'
        fixture.write_text('''#!/usr/bin/env python3
import json
from pathlib import Path
import sys
root = Path(__file__).parent
counts = {}
for line in sys.stdin:
    r = json.loads(line)
    with (root / 'navigation.requests').open('a') as out:
        out.write(json.dumps(r) + '\\n')
    k = r['type']
    counts[k] = counts.get(k, 0) + 1
    e = {'id': r.get('id', 0)}
    location = {'uri': (root / 'unicode.rs').as_uri(), 'line': 0, 'character': 3}
    item = dict(location, name='target', kind='Function')
    if k == 'initialize':
        e.update(type='initialized')
    elif k in ('textDocument/implementation', 'textDocument/typeDefinition'):
        locations = [location]
        if counts[k] > 1:
            locations.append(dict(location, line=1))
        e.update(type=k.split('/')[-1], locations=locations)
    elif k == 'textDocument/prepareCallHierarchy':
        e.update(type='callHierarchyPrepare', items=[dict(item, raw={'name': 'opaque-target', 'data': {'token': 17}})])
    elif k in ('callHierarchy/incomingCalls', 'callHierarchy/outgoingCalls'):
        e.update(type=k.split('/')[-1], calls=[{'item': item}, {'item': dict(item, line=1, name='second')}])
    elif k == 'textDocument/prepareTypeHierarchy':
        e.update(type='typeHierarchyPrepare', items=[dict(item, raw={'name': 'opaque-type', 'data': {'token': 29}})])
    elif k in ('typeHierarchy/supertypes', 'typeHierarchy/subtypes'):
        e.update(type=k.split('/')[-1], items=[item, dict(item, line=1, name='second')])
    elif k == 'workspace/symbol':
        e.update(type='workspaceSymbol', symbols=[dict(item, detail=location['uri'])])
    elif k == 'textDocument/signatureHelp':
        signatures = [{'label': 'target(value: int)', 'documentation': 'Fixture signature'}] if counts[k] == 1 else []
        e.update(type='signatureHelp', signatures=signatures)
    elif k == 'shutdown':
        break
    else:
        continue
    print(json.dumps(e), flush=True)
''', encoding='utf-8')
        fixture.chmod(0o700)
        e = self.editor(editor_class=PopupEditor, extra_setup=(
            'let g:simplecc_daemon_path = ' + vim_string(fixture) + '\n'
            'let g:simplecc_signature_help = 0\n'
            'autocmd VimEnter * setfiletype rust | SimpleCCStart\n'))
        # The request log precedes the reply: wait for the real provider's
        # initialized handler rather than relying on daemon scheduling.
        self.until(e, lambda: self.capture(e, "get(g:, 'simplecc_status', '')") == 'ready')
        return e

    def requests(self):
        path = self.root / 'navigation.requests'
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def until(self, e, predicate):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            value = predicate()
            if value:
                return value
            e.pump(0.05)
        self.fail('Timed out waiting for real SimpleCC result; requests: ' + repr(self.requests()))

    def test_implementation_and_type_definition_unicode_jump_and_multiple_results(self):
        e = self.navigation_editor()
        for action in ('simple_implementation', 'simple_type_definition'):
            with self.subTest(action=action):
                self.assertIn(action, e.snapshot()['capabilities'])
                self.workbench(e, action)
                e.state_where(lambda s: s.get('file', '').endswith('unicode.rs'))
                self.assertEqual(self.capture(e, "[line('.'), col('.') ]"), [1, 8])
                # Cursor after 中 + supplementary-plane emoji must be sent as
                # three UTF-16 code units, not seven UTF-8 bytes.
                self.workbench(e, action)
                e.state_where(lambda s: s.get('filetype') == 'qf')
                items = self.capture(e, 'getloclist(0)')
                self.assertEqual([(x['lnum'], x['col']) for x in items], [(1, 8), (2, 8)])
                self.assertEqual([x['user_data']['character'] for x in items], [3, 3])
                e.keys(b'j\r')
                e.state_where(lambda s: s.get('filetype') != 'qf' and s.get('line') == 2)
                self.assertEqual(self.capture(e, "[line('.'), col('.') ]"), [2, 8])
                kind = 'textDocument/' + ('implementation' if action.endswith('implementation') else 'typeDefinition')
                sent = [r for r in self.requests() if r['type'] == kind]
                self.assertEqual(len(sent), 2)
                self.assertEqual(sent[-1]['character'], 3)

    def test_incoming_and_outgoing_follow_prepare_with_opaque_item_and_real_list(self):
        e = self.navigation_editor()
        for direction in ('incoming', 'outgoing'):
            with self.subTest(direction=direction):
                self.workbench(e, 'simple_' + direction + '_calls')
                e.state_where(lambda s: s.get('filetype') == 'qf')
                items = self.capture(e, 'getqflist()')
                self.assertEqual([i['text'] for i in items], ['[Function] target', '[Function] second'])
                self.assertEqual([i['col'] for i in items], [8, 8])
                chain = [r for r in self.requests() if 'CallHierarchy' in r['type'] or r['type'].startswith('callHierarchy/')]
                self.assertEqual([r['type'] for r in chain[-2:]], ['textDocument/prepareCallHierarchy', 'callHierarchy/' + direction + 'Calls'])
                self.assertEqual(chain[-1]['item'], {'name': 'opaque-target', 'data': {'token': 17}})
                e.keys(b'j\r')
                e.state_where(lambda s: s.get('filetype') != 'qf' and s.get('line') == 2)
                self.assertEqual(self.capture(e, "[line('.'), col('.') ]"), [2, 8])

    def test_type_hierarchies_preserve_opaque_prepare_item_and_jump(self):
        e = self.navigation_editor()
        for direction in ('supertypes', 'subtypes'):
            with self.subTest(direction=direction):
                self.workbench(e, 'simple_' + direction)
                e.state_where(lambda s: s.get('filetype') == 'qf')
                items = self.capture(e, 'getqflist()')
                self.assertEqual([(i['lnum'], i['col']) for i in items], [(1, 8), (2, 8)])
                chain = [r for r in self.requests() if 'TypeHierarchy' in r['type'] or r['type'].startswith('typeHierarchy/')]
                self.assertEqual([r['type'] for r in chain[-2:]], ['textDocument/prepareTypeHierarchy', 'typeHierarchy/' + direction])
                self.assertEqual(chain[-1]['item'], {'name': 'opaque-type', 'data': {'token': 29}})
                e.keys(b'j\r')
                e.state_where(lambda s: s.get('filetype') != 'qf' and s.get('line') == 2)
                self.assertEqual(self.capture(e, "[line('.'), col('.') ]"), [2, 8])

    def test_live_workspace_symbols_compatibility_input_accept_and_cancel(self):
        e = self.navigation_editor()
        for accept in (True, False):
            with self.subTest(accept=accept):
                before = e.snapshot()
                e.screen.clear()
                self.workbench(e, 'simple_workspace_symbols')
                self.assert_pty_text(e, b'Workspace Symbol')
                frame = e.gui_frame()
                self.assertFalse(frame['supported'])
                self.assertIn('popup', frame['fallback_reason'].lower())
                # A filtered popup must remain in the compatibility view.
                self.assertFalse(frame.get('popups', []))
                count = sum(r['type'] == 'workspace/symbol' for r in self.requests())
                e.keys('ta')
                self.until(e, lambda: sum(r['type'] == 'workspace/symbol' for r in self.requests()) > count)
                self.assert_pty_text(e, b'target')
                self.assertEqual([r for r in self.requests() if r['type'] == 'workspace/symbol'][-1]['query'], 'ta')
                e.keys(b'\r' if accept else b'\x1b')
                self.until(e, lambda: e.gui_frame()['supported'])
                self.assertTrue(e.gui_frame()['supported'])
                after = e.snapshot()
                if accept:
                    self.assertTrue(after['file'].endswith('unicode.rs'))
                    self.assertEqual([after['line'], after['column']], [1, 8])
                else:
                    self.assertEqual([after[k] for k in ('buffer', 'line', 'column', 'changedtick')],
                                     [before[k] for k in ('buffer', 'line', 'column', 'changedtick')])

    def test_stale_cursor_and_disabled_provider_do_not_send_semantic_requests(self):
        e = self.navigation_editor()
        source = e.snapshot()
        e.keys('j')
        e.state_where(lambda s: s.get('line') == 2)
        before = sum(r['type'] == 'textDocument/implementation' for r in self.requests())
        reply = e.request('workbench', command='simple_implementation', ok=False,
                          **{k: source[k] for k in ('window', 'buffer', 'changedtick', 'line', 'column')})
        self.assertIn('cursor', reply['error'].lower())
        self.assertEqual(sum(r['type'] == 'textDocument/implementation' for r in self.requests()), before)
        e.ex('let g:vimrc_workbench_enable = 0')
        state = e.snapshot()
        self.assertNotIn('simple_implementation', state['capabilities'])
        self.workbench(e, 'simple_implementation', ok=False)
        self.assertEqual(sum(r['type'] == 'textDocument/implementation' for r in self.requests()), before)

    def test_signature_action_paints_native_popup_and_empty_reply_closes_it(self):
        e = self.navigation_editor()
        original = e.snapshot()['buffer']
        self.workbench(e, 'simple_signature_help')
        self.until(e, lambda: self.capture(e, 'len(popup_list())') == 1)
        frame = e.gui_frame()
        self.assertTrue(frame['supported'], frame.get('fallback_reason'))
        self.assertEqual(len(frame['popups']), 1)
        self.assertIn('target(value: int)', ''.join(r['text'] for r in frame['popups'][0]['runs']))
        self.workbench(e, 'simple_signature_help')
        self.until(e, lambda: self.capture(e, 'len(popup_list())') == 0)
        self.assertEqual(e.gui_frame()['popups'], [])
        self.assertEqual(e.snapshot()['buffer'], original)
        self.assertEqual(sum(r['type'] == 'textDocument/signatureHelp' for r in self.requests()), 2)
        e.keys(b'A!\x1b')
        e.state_where(lambda s: s.get('modified'))
        self.assertEqual(self.capture(e, 'getline(1)'), 'original line!')


if __name__ == '__main__':
    unittest.main(verbosity=2)
