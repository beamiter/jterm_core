#!/usr/bin/env python3
"""Real vimrc mappings and SimpleCC requests; offline daemon wire fixture."""
import json
import time
import unittest

import test_vimrc_workbench as harness
from test_render_popups import PopupEditor
from test_vimrc_workbench import AVAILABLE, vim_string


@unittest.skipUnless(AVAILABLE, 'Needs real vimrc and Simple plugins')
class SemanticEditingTests(unittest.TestCase):
    setUp = harness.VimrcWorkbenchTests.setUp
    tearDown = harness.VimrcWorkbenchTests.tearDown
    editor = harness.VimrcWorkbenchTests.editor
    capture = harness.VimrcWorkbenchTests.capture

    def requests(self, kind=None):
        p = self.root / 'editing.requests'
        rows = [json.loads(s) for s in p.read_text().splitlines()] if p.exists() else []
        return [r for r in rows if kind is None or r['type'] == kind]

    def until(self, e, predicate):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if predicate():
                return
            e.pump(.03)
        self.fail('No expected daemon result: ' + repr(self.requests()))

    def editing_editor(self):
        daemon = self.root / 'editing-daemon'
        daemon.write_text('''#!/usr/bin/env python3
import json, sys, threading, time
from pathlib import Path
root = Path(__file__).parent
def emit(e):
    print(json.dumps(e), flush=True)
def release_range_reply(e):
    deadline = time.monotonic() + 5
    while not (root / 'format.release').exists():
        if time.monotonic() > deadline:
            return
        time.sleep(.01)
    emit(e)
    (root / 'format.reply.sent').write_text(str(e['id']))
for line in sys.stdin:
    r = json.loads(line)
    with (root / 'editing.requests').open('a') as out:
        out.write(json.dumps(r) + '\\n')
    k = r['type']
    e = {'id': r.get('id', 0)}
    if k == 'initialize': e.update(type='initialized', capabilities={'code_action_identity': True})
    elif k in ('textDocument/definition', 'textDocument/references'):
        e.update(type=k.split('/')[-1], locations=[{'uri': r['uri'], 'line': 1, 'character': 0}])
    elif k == 'textDocument/prepareRename':
        e.update(type='prepareRename', result={'placeholder': 'original', 'default_behavior': True})
    elif k == 'textDocument/signatureHelp':
        e.update(type='signatureHelp', signatures=[{'label': 'fixture(value: int)'}])
    elif k == 'textDocument/codeAction': e.update(type='codeAction', generation=r['id'], uri=r['uri'], actions=[])
    elif k in ('textDocument/rangeFormatting', 'textDocument/formatting'):
        e.update(type='formatting', edits=[{'line': 0, 'character': 0, 'end_line': 0, 'end_character': 8, 'new_text': 'FORMATTED'}])
        if k == 'textDocument/rangeFormatting':
            threading.Thread(target=release_range_reply, args=(e,), daemon=True).start()
        else:
            emit(e)
        continue
    elif k == 'shutdown': break
    else: continue
    emit(e)
''')
        daemon.chmod(0o700)
        e = self.editor(editor_class=PopupEditor, extra_setup=(
            'let g:simplecc_daemon_path = ' + vim_string(daemon) + '\n'
            'let g:simplecc_signature_help = 0\n'
            'autocmd VimEnter * setfiletype rust | call g:VimrcConfigureFiletype() | SimpleCCStart\n'))
        self.until(e, lambda: self.capture(e, "get(g:, 'simplecc_status', '')") == 'ready')
        return e

    def test_first_visual_range_and_stale_format_reply(self):
        e = self.editing_editor()
        # No previous Visual selection: ':' must commit the current marks.
        self.assertEqual(self.capture(e, "line(\"'<\")"), 0)
        e.keys('ggVj cf')
        self.until(e, lambda: bool(self.requests('textDocument/rangeFormatting')))
        r = self.requests('textDocument/rangeFormatting')[-1]
        self.assertEqual([r[k] for k in ('line', 'character', 'end_line', 'end_character')], [0, 0, 1, 11])
        e.keys(b'ggA!\x1b')
        e.state_where(lambda state: state.get('modified') and state.get('mode') == 'n')
        self.assertEqual(self.capture(e, 'getline(1)'), 'original line!')
        # Release the reply only after Vim has applied the intervening edit.
        # A loaded CI worker cannot race a fixed timer before that edit.
        (self.root / 'format.release').write_text('go')
        self.until(e, lambda: (self.root / 'format.reply.sent').exists())
        e.pump(.05)
        self.assertEqual(self.capture(e, 'getline(1)'), 'original line!')
        e.keys('ggVj ca')
        self.until(e, lambda: bool(self.requests('textDocument/codeAction')))
        r = self.requests('textDocument/codeAction')[-1]
        self.assertEqual([r[k] for k in ('line', 'character', 'end_line', 'end_character')], [0, 0, 1, 11])
        e.ex('SimpleCCFormat')
        self.until(e, lambda: bool(self.requests('textDocument/formatting')))
        self.until(e, lambda: self.capture(e, 'getline(1)').startswith('FORMATTED'))

    def test_function_keys_and_insert_signature_keep_text(self):
        e = self.editing_editor()
        e.keys(b'\x1b[24~')
        self.until(e, lambda: bool(self.requests('textDocument/definition')))
        self.assertEqual(self.capture(e, "line('.')"), 2)
        e.keys(b'\x1b[24;2~')
        self.until(e, lambda: bool(self.requests('textDocument/references')))
        e.state_where(lambda s: s.get('filetype') == 'qf')
        e.keys(b'\r')
        e.state_where(lambda s: s.get('filetype') != 'qf')
        e.keys(b'\x1bOQ')
        e.state_where(lambda s: s.get('mode', '').startswith('c'))
        e.keys(b'\x1b')
        e.state_where(lambda s: s.get('mode') == 'n')
        before = e.snapshot()
        e.keys(b'i\x07s')
        self.until(e, lambda: bool(self.requests('textDocument/signatureHelp')))
        after = e.snapshot()
        self.assertTrue(after['mode'].startswith('i'))
        self.assertEqual(after['changedtick'], before['changedtick'])
        e.keys(b'\x1b')
        self.assertEqual(self.capture(e, "getline(1, '$')"), ['original line', 'second line'])


if __name__ == '__main__':
    unittest.main()
