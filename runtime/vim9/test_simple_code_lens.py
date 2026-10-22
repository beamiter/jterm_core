#!/usr/bin/env python3
"""Real vimrc/SimpleCC/native bridge CodeLens lifecycle, without an LSP server.

The daemon fixture supplies asynchronous protocol replies. All requests,
properties, named actions, cache checks and paint updates use shipped code.
Use the same sibling checkouts/environment as test_vimrc_workbench.py.
"""

import json
import time
import unittest

import test_vimrc_workbench as harness
from test_render_popups import PopupEditor


@unittest.skipUnless(harness.AVAILABLE, "Needs Vim and vimrc/simple* checkouts")
class SimpleCodeLensTests(unittest.TestCase):
    setUp = harness.VimrcWorkbenchTests.setUp
    tearDown = harness.VimrcWorkbenchTests.tearDown
    editor = harness.VimrcWorkbenchTests.editor
    capture = harness.VimrcWorkbenchTests.capture
    workbench = harness.VimrcWorkbenchTests.workbench
    assert_pty_text = harness.VimrcWorkbenchTests.assert_pty_text

    def start(self):
        e = self.editor(editor_class=PopupEditor, extra_setup="""
let g:simplecc_inlay_hints = 0
let g:simplecc_semantic_tokens = 0
let g:simplecc_virtual_diag = 0
let g:simplecc_diag_float = 0
let g:simplecc_signature_help = 0
let g:test_decorations = 0
autocmd User SimpleCCDecorationsChanged let g:test_decorations += 1
""")
        self.e = e
        self.mode = self.root / "lens-mode"
        self.mode.write_text("lens", encoding="utf-8")
        self.log = self.root / "lens-requests.jsonl"
        fixture = self.root / "code-lens-daemon"
        fixture.write_text('''#!/usr/bin/env python3
import json
from pathlib import Path
import sys
import time
root = Path(__file__).parent
for line in sys.stdin:
    request = json.loads(line)
    with (root / 'lens-requests.jsonl').open('a') as output:
        output.write(json.dumps(request) + '\\n')
    kind = request['type']
    reply = {'id': request.get('id', 0)}
    if kind == 'initialize':
        reply.update(type='initialized')
    elif kind == 'textDocument/codeLens':
        mode = (root / 'lens-mode').read_text().strip()
        time.sleep(0.12)
        reply.update(type='codeLens', generation=request['id'], uri=request['uri'],
                     lenses=[] if mode == 'empty' else [{
                         'line': 0, 'character': 0, 'end_line': 0,
                         'end_character': 8, 'command_title': 'Run 界é tests', 'index': 0}])
        if mode == 'legacy':
            reply.pop('generation')
            reply.pop('uri')
    elif kind == 'codeLens/execute':
        reply.update(type='codeLensExecute', generation=request['generation'],
                     uri=request['uri'])
    elif kind == 'shutdown':
        break
    else:
        continue
    print(json.dumps(reply, ensure_ascii=False), flush=True)
''', encoding="utf-8")
        fixture.chmod(0o700)
        e.ex("set nowrap nolist nospell nonumber norelativenumber nocursorline nocursorcolumn")
        e.ex("highlight Comment guifg=#8a9bae guibg=NONE gui=italic")
        e.ex("let g:simplecc_daemon_path = " + harness.vim_string(fixture)
             + " | setfiletype rust | SimpleCCStart")
        self.wait_requests(lambda requests: any(r['type'] == 'textDocument/didOpen' for r in requests))
        self.source = e.gui_frame()
        self.assertTrue(self.source['supported'], self.source.get('fallback_reason'))
        # An idle provider response must trigger paint itself, without relying
        # on the bridge watchdog or an explicit set_render/snapshot request.
        e.ex("let g:test_stopped_polls = 0 | for t in timer_info() | if t.time == 750 && string(t.callback) =~# '_Poll' | call timer_stop(t.id) | let g:test_stopped_polls += 1 | endif | endfor")
        self.assertEqual(self.capture(e, 'g:test_stopped_polls'), 1)
        self.source = e.gui_frame()
        return e

    def requests(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines() if line]

    def wait_requests(self, predicate):
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            requests = self.requests()
            if predicate(requests):
                return requests
            self.e.pump(0.02)
        self.fail(f"Expected daemon request was not sent: {self.requests()}")

    @staticmethod
    def labels(frame):
        return ''.join(run['text'] for run in frame['runs'] if run['line'] == 0)

    def lens(self):
        self.workbench(self.e, 'simple_code_lens')
        return self.e.painted(lambda f: f['supported'] and 'Run 界é tests' in self.labels(f))

    def daemon_barrier(self):
        count = sum(r['type'] == 'textDocument/codeLens' for r in self.requests())
        self.workbench(self.e, 'simple_code_lens')
        self.wait_requests(lambda rs: sum(r['type'] == 'textDocument/codeLens' for r in rs) > count)

    def test_async_named_action_paints_and_clears_without_editing_source(self):
        e = self.start()
        state = e.snapshot()
        for action in ('simple_code_lens', 'simple_code_lens_run', 'simple_inlay_hints'):
            self.assertIn(action, state['capabilities'])
        frame = self.lens()
        self.assertEqual(frame['changedtick'], self.source['changedtick'])
        self.assertEqual(frame['buffer'], self.source['buffer'])
        self.assertEqual(frame['window'], self.source['window'])
        glyph = next(run for run in frame['runs'] if run['text'] == '界')
        self.assertEqual((glyph['line'], glyph['byte_start'], glyph['byte_end'], glyph['cells']), (0, 0, 0, 2))
        self.assertEqual(glyph['style']['foreground'], 0x8a9bae)
        self.assertTrue(glyph['style']['italic'])
        self.assertGreater(self.capture(e, 'g:test_decorations'), 0)
        self.assertEqual(self.capture(e, "getline(1, '$')"), ['original line', 'second line'])

        self.mode.write_text('empty', encoding='utf-8')
        self.workbench(e, 'simple_code_lens')
        cleared = e.painted(lambda f: f['supported'] and f['revision'] > frame['revision']
                            and 'Run 界é tests' not in self.labels(f))
        self.assertEqual(cleared['changedtick'], frame['changedtick'])
        self.assertEqual(self.capture(e, "prop_list(1)"), [])

    def test_run_binds_request_generation_and_refuses_cross_buffer_or_edited_cache(self):
        e = self.start()
        self.lens()
        original = e.snapshot()
        request = next(r for r in reversed(self.requests()) if r['type'] == 'textDocument/codeLens')
        self.workbench(e, 'simple_code_lens_run')
        requests = self.wait_requests(lambda rs: any(r['type'] == 'codeLens/execute' for r in rs))
        executed = next(r for r in requests if r['type'] == 'codeLens/execute')
        self.assertEqual((executed['uri'], executed['generation'], executed['index']),
                         (request['uri'], request['id'], 0))
        count = len([r for r in requests if r['type'] == 'codeLens/execute'])

        e.request('open', path=str(e.other))
        e.ex('setfiletype rust')
        self.workbench(e, 'simple_code_lens_run', state=original, ok=False)
        self.workbench(e, 'simple_code_lens_run')
        e.request('open', path=str(e.file))
        e.ex("call setline(1, 'edited source')")
        self.workbench(e, 'simple_code_lens_run')
        self.daemon_barrier()
        self.assertEqual(len([r for r in self.requests() if r['type'] == 'codeLens/execute']), count)

    def test_legacy_daemon_lenses_display_but_cannot_execute(self):
        e = self.start()
        self.mode.write_text('legacy', encoding='utf-8')
        self.lens()
        self.workbench(e, 'simple_code_lens_run')
        self.assert_pty_text(e, b'Rebuild the SimpleCC daemon')
        self.daemon_barrier()
        self.assertFalse(any(r['type'] == 'codeLens/execute' for r in self.requests()))


if __name__ == '__main__':
    unittest.main(verbosity=2)
