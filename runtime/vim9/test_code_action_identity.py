#!/usr/bin/env python3
"""Real Vim menu selection and authenticated bridge with identity-aware replies."""
import json
import unittest

import test_vimrc_workbench as harness
from test_render_popups import PopupEditor
import test_semantic_editing


@unittest.skipUnless(harness.AVAILABLE, 'Needs Vim, vimrc and Simple plugins')
class CodeActionIdentityTests(unittest.TestCase):
    setUp = harness.VimrcWorkbenchTests.setUp
    tearDown = harness.VimrcWorkbenchTests.tearDown
    editor = harness.VimrcWorkbenchTests.editor
    capture = harness.VimrcWorkbenchTests.capture
    until = test_semantic_editing.SemanticEditingTests.until

    def requests(self, kind=None):
        path = self.root / 'action.requests'
        rows = [json.loads(s) for s in path.read_text().splitlines()] if path.exists() else []
        return [r for r in rows if kind is None or r['type'] == kind]

    def start(self, *, legacy=False):
        if legacy:
            (self.root / 'legacy').touch()
        daemon = self.root / 'action-daemon'
        daemon.write_text('''#!/usr/bin/env python3
import json, sys, threading, time
from pathlib import Path
root = Path(__file__).parent
lock = threading.Lock()
def emit(event):
    with lock:
        print(json.dumps(event), flush=True)
def release(event, request):
    while not (root / ('release-' + str(request['id']))).exists():
        time.sleep(.01)
    emit(event)
    emit({'type': 'diagnostics', 'uri': request['uri'], 'server': 'delivery-sentinel',
          'items': [{'line': 0, 'character': 0, 'severity': 4,
                     'message': 'delivered ' + str(request['id'])}]})
for line in sys.stdin:
    r = json.loads(line)
    with (root / 'action.requests').open('a') as log:
        log.write(json.dumps(r) + '\\n')
    kind = r['type']
    event = {'id': r.get('id', 0)}
    if kind == 'initialize':
        event.update(type='initialized', capabilities={'code_action_identity': not (root / 'legacy').exists()})
    elif kind == 'textDocument/codeAction':
        event.update(type='codeAction', generation=r['id'], uri=r['uri'], actions=[{
            'title': 'Replace original', 'kind': 'quickfix', 'index': 7}])
    elif kind == 'textDocument/executeAction':
        event.update(type='executeAction', generation=r['generation'], uri=r['uri'], edit={'changes': [{
            'uri': r['uri'], 'edits': [{'line': 0, 'character': 0, 'end_line': 0,
            'end_character': 8, 'new_text': 'UPDATED'}]}]})
    elif kind == 'shutdown':
        break
    else:
        continue
    if kind == 'textDocument/executeAction':
        threading.Thread(target=release, args=(event, r), daemon=True).start()
    else:
        emit(event)
''', encoding='utf-8')
        daemon.chmod(0o700)
        e = self.editor(editor_class=PopupEditor, extra_setup=(
            'let g:simplecc_daemon_path = ' + harness.vim_string(daemon) + '\n'
            'let g:simplecc_signature_help = 0\n'
            'autocmd VimEnter * setfiletype rust | call g:VimrcConfigureFiletype() | SimpleCCStart\n'))
        self.until(e, lambda: self.capture(e, "get(g:, 'simplecc_status', '')") == 'ready')
        self.assertTrue(e.gui_frame()['supported'])
        self.delivery = self.root / 'delivery.json'
        source = e.snapshot()['buffer']
        expression = ("{'items': simplecc#DiagnosticSnapshot(" + str(source)
                      + ").items, 'line': getline(1), 'popups': popup_list()}")
        e.ex("autocmd User SimpleCCDiagnosticsChanged call writefile([json_encode("
             + expression + ")], " + harness.vim_string(self.delivery) + ")")
        return e

    def choose(self, e):
        # Authenticated native action opens the real popup, which owns PTY Enter.
        source = e.snapshot()
        request_id = e.next_id
        e.next_id += 1
        e.send({'id': request_id, 'action': 'workbench', 'command': 'simple_code_action',
                **{k: source[k] for k in ('window', 'buffer', 'changedtick', 'line', 'column')}})
        e.wait(lambda m: m.get('event') == 'render' and not m.get('supported', False)
               and 'popup' in m.get('fallback_reason', ''))
        e.keys(b'\r')
        self.assertTrue(e.wait(lambda m: m.get('event') == 'response'
                               and m.get('id') == request_id)['ok'])
        self.until(e, lambda: bool(self.requests('textDocument/executeAction')))
        action = self.requests('textDocument/codeAction')[-1]
        execution = self.requests('textDocument/executeAction')[-1]
        self.assertEqual(execution['generation'], action['id'])
        self.assertEqual(execution['uri'], action['uri'])
        self.assertEqual(execution['index'], 7)
        e.wait(lambda m: m.get('event') == 'render' and m.get('supported', False))
        return execution

    def release(self, e, request):
        (self.root / ('release-' + str(request['id']))).touch()
        marker = 'delivered ' + str(request['id'])
        def delivered():
            if not self.delivery.exists():
                return False
            try:
                state = json.loads(self.delivery.read_text())
            except json.JSONDecodeError:
                return False
            return any(item.get('text') == marker for item in state['items'])
        self.until(e, delivered)
        return json.loads(self.delivery.read_text())

    def test_real_selection_applies_identity_bound_edit(self):
        e = self.start()
        execution = self.choose(e)
        self.release(e, execution)
        self.until(e, lambda: self.capture(e, 'getline(1)') == 'UPDATED line')
        self.assertEqual(self.capture(e, 'getline(2)'), 'second line')
        self.assertTrue(e.gui_frame()['supported'])

    def test_document_change_rejects_late_selected_edit(self):
        e = self.start()
        execution = self.choose(e)
        source = e.snapshot()
        e.keys(b'A!\x1b')
        e.state_where(lambda s: s.get('mode') == 'n' and s.get('changedtick', 0) > source['changedtick'])
        delivered = self.release(e, execution)
        self.assertEqual(delivered['line'], 'original line!')
        self.assertEqual(self.capture(e, 'getline(1)'), 'original line!')
        self.assertTrue(e.gui_frame()['supported'])

    def test_legacy_daemon_cannot_receive_unbound_execution(self):
        e = self.start(legacy=True)
        harness.VimrcWorkbenchTests.workbench(self, e, 'simple_code_action')
        # The following Ex command observes completion of the native action.
        self.assertIn('update simplecc-daemon for safe code actions', self.capture(e, 'execute("messages")'))
        self.assertFalse(self.requests('textDocument/codeAction'))
        self.assertFalse(self.requests('textDocument/executeAction'))
        self.assertTrue(e.gui_frame()['supported'])


if __name__ == '__main__':
    unittest.main()
