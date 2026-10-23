#!/usr/bin/env python3
"""Real SimpleCC daemon-to-workbench diagnostics integration (no LSP server)."""
from __future__ import annotations

import unittest

import test_vimrc_workbench as harness


@unittest.skipUnless(harness.AVAILABLE, "Needs Vim and vimrc/simple* checkouts")
class SimpleDiagnosticsTests(unittest.TestCase):
    # Share lifecycle and authenticated PTY helpers without inheriting the
    # existing suite's test methods (and therefore running them twice).
    setUp = harness.VimrcWorkbenchTests.setUp
    tearDown = harness.VimrcWorkbenchTests.tearDown
    editor = harness.VimrcWorkbenchTests.editor
    capture = harness.VimrcWorkbenchTests.capture
    workbench = harness.VimrcWorkbenchTests.workbench

    def diagnostic_editor(self):
        e = self.editor()
        e.ex("call setline(1, ['中😀alpha', 'second line']) | write")
        fixture = self.root / "diagnostics-daemon"
        fixture.write_text('''#!/usr/bin/env python3
import json
import sys
import time
for line in sys.stdin:
    request = json.loads(line)
    kind = request['type']
    if kind == 'initialize':
        print(json.dumps({'type': 'initialized', 'id': request['id']}), flush=True)
    elif kind == 'textDocument/didOpen':
        items = []
        if request['uri'].endswith('/main.txt'):
            items = [
                {'line': 0, 'character': 1, 'severity': 1, 'message': 'before emoji'},
                {'line': 0, 'character': 3, 'severity': 2, 'message': 'after emoji'},
                {'line': 1, 'character': 2, 'severity': 3, 'message': 'second info'},
            ]
        print(json.dumps({'type': 'diagnostics', 'uri': request['uri'], 'items': items}), flush=True)
    elif kind == 'textDocument/didSave':
        time.sleep(0.15)
        print(json.dumps({'type': 'diagnostics', 'uri': request['uri'], 'items': []}), flush=True)
    elif kind == 'shutdown':
        break
''', encoding="utf-8")
        fixture.chmod(0o700)
        e.ex("let g:simplecc_daemon_path = " + harness.vim_string(fixture))
        e.ex("setfiletype rust | SimpleCCStart")
        # A fast daemon can publish during ex()'s wait for Normal mode. That
        # same state is already consumed by the helper and need not be emitted
        # again until it changes. Observe it before waiting for another event.
        ready = lambda s: len(s.get('diagnostics', [])) == 3
        state = e.state if ready(e.state) else e.state_where(ready)
        return e, state

    def test_snapshot_uses_current_buffer_cache_and_byte_columns(self):
        e, state = self.diagnostic_editor()
        items = {item['text']: item for item in state['diagnostics']}
        self.assertEqual(set(items), {'before emoji', 'after emoji', 'second info'})
        self.assertEqual((items['before emoji']['line'], items['before emoji']['column']), (1, 4))
        self.assertEqual((items['after emoji']['line'], items['after emoji']['column']), (1, 8))
        self.assertEqual((items['second info']['line'], items['second info']['column']), (2, 3))
        self.assertEqual(items['before emoji']['kind'], 'error')
        self.assertEqual(items['after emoji']['kind'], 'warning')
        self.assertEqual(items['second info']['kind'], 'info')
        self.assertEqual(self.capture(e, '[len(getqflist()), len(getloclist(0))]'), [0, 0])
        self.assertFalse(self.capture(e, "!empty(filter(getwininfo(), 'v:val.quickfix'))"))
        provided = self.capture(e, 'g:VimrcWorkbenchDiagnostics()')
        self.assertEqual(provided['total'], 3)
        self.assertEqual(len(provided['items']), 3)
        e.request('open', path=str(e.other))
        other = e.snapshot()
        self.assertEqual(other['diagnostics'], [])
        self.assertEqual(self.capture(e, 'g:VimrcWorkbenchDiagnostics().total'), 0)
        e.request('open', path=str(e.file))
        restored = e.snapshot()
        self.assertEqual(len(restored['diagnostics']), 3)
        target = items['after emoji']
        e.request('open', path=target['filename'], line=target['line'], column=target['column'])
        self.assertEqual(self.capture(e, "[line('.'), col('.') ]"), [1, 8])
        e.ex('let g:vimrc_workbench_enable = 0')
        self.assertEqual(e.snapshot()['diagnostics'], [])
        e.ex('let g:vimrc_workbench_enable = 1')
        self.assertEqual(len(e.snapshot()['diagnostics']), 3)

    def test_async_clear_invalidates_bridge_without_a_list_window(self):
        e, state = self.diagnostic_editor()
        self.assertEqual(len(state['diagnostics']), 3)
        # The daemon publishes an empty update after didSave. Wait for an
        # unsolicited bridge event, without forcing a snapshot or opening UI.
        e.ex('write')
        is_clear = lambda s: s.get('diagnostics') == [] and s.get(
            'workbench', {}).get('diagnostics', {}).get('error') == 0
        cleared = e.state if is_clear(e.state) else e.state_where(is_clear)
        self.assertEqual(cleared['buffer'], state['buffer'])
        self.assertEqual(self.capture(e, 'g:VimrcWorkbenchDiagnostics().total'), 0)
        self.assertEqual(self.capture(e, '[len(getqflist()), len(getloclist(0))]'), [0, 0])

    def test_fixed_workbench_actions_use_real_diagnostic_navigation(self):
        e, state = self.diagnostic_editor()
        for action in ('simple_diagnostics', 'simple_next_diagnostic', 'simple_previous_diagnostic'):
            self.assertIn(action, state['capabilities'])
        e.ex('call cursor(1, 1)')
        self.workbench(e, 'simple_next_diagnostic')
        e.state_where(lambda s: s.get('line') == 1 and s.get('column') == 4)
        self.workbench(e, 'simple_next_diagnostic')
        e.state_where(lambda s: s.get('line') == 1 and s.get('column') == 8)
        self.workbench(e, 'simple_previous_diagnostic')
        e.state_where(lambda s: s.get('line') == 1 and s.get('column') == 4)
        self.workbench(e, 'simple_diagnostics')
        self.assertTrue(self.capture(e, "!empty(filter(getwininfo(), 'v:val.quickfix'))"))
        self.assertEqual(self.capture(e, 'len(getloclist(0))'), 3)

    def test_folded_readonly_scratch_diagnostic_jump_reveals_and_returns(self):
        e, _ = self.diagnostic_editor()
        e.ex("call setline(3, 'outside folded code')")
        e.ex('setlocal buftype=acwrite readonly foldmethod=manual foldenable | 1,2fold | normal! 3G')
        e.ex('clearjumps')
        before = e.snapshot()
        self.assertEqual(self.capture(e, 'foldclosed(2)'), 1)
        self.assertFalse(e.gui_frame()['supported'])
        self.workbench(e, 'simple_previous_diagnostic')
        e.state_where(lambda state: state['line'] == 2 and state['column'] == 3)
        self.assertEqual(self.capture(e, 'foldclosed(2)'), -1)
        self.assertTrue(e.gui_frame()['supported'])
        e.keys(b'\x0f')
        returned = e.state_where(lambda state: state['line'] == 3 and state['column'] == before['column'])
        for field in ('window', 'buffer', 'changedtick'):
            self.assertEqual(returned[field], before[field], field)
        self.assertEqual(self.capture(e, 'getline(1)'), '中😀alpha')
        self.assertEqual(self.capture(e, '&readonly'), 1)
        e.keys(b'\x09')
        e.state_where(lambda state: state['line'] == 2 and state['column'] == 3)


if __name__ == '__main__':
    unittest.main(verbosity=2)
