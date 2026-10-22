#!/usr/bin/env python3
"""Real SimpleCC inlay replies through vimrc actions and native Vim paint.

Only the daemon replies are synthetic. Source positions, text properties,
invalidation, named actions and row deltas use the actual plugin and bridge.
The background bridge poll is stopped to require provider-driven repaint.
"""

import json
import time
import unittest

import test_vimrc_workbench as harness
from test_render_popups import PopupEditor


@unittest.skipUnless(harness.AVAILABLE, "Needs Vim and vimrc/simple* checkouts")
class SimpleInlayHintsTests(unittest.TestCase):
    setUp = harness.VimrcWorkbenchTests.setUp
    tearDown = harness.VimrcWorkbenchTests.tearDown
    editor = harness.VimrcWorkbenchTests.editor
    capture = harness.VimrcWorkbenchTests.capture
    workbench = harness.VimrcWorkbenchTests.workbench

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
        self.mode = self.root / "inlay-mode"
        self.mode.write_text("hints", encoding="utf-8")
        self.log = self.root / "inlay-requests.jsonl"
        fixture = self.root / "inlay-daemon"
        fixture.write_text('''#!/usr/bin/env python3
import json
from pathlib import Path
import sys
import time
root = Path(__file__).parent
for line in sys.stdin:
    request = json.loads(line)
    with (root / 'inlay-requests.jsonl').open('a') as output:
        output.write(json.dumps(request) + '\\n')
    kind = request['type']
    reply = {'id': request.get('id', 0)}
    if kind == 'initialize':
        reply.update(type='initialized')
    elif kind == 'textDocument/inlayHint':
        mode = (root / 'inlay-mode').read_text().strip()
        time.sleep(0.12)
        reply.update(type='inlayHint', hints=[] if mode == 'empty' else [
            {'line': 0, 'character': 3, 'label': ': 型é',
             'padding_left': True, 'padding_right': True},
            {'line': 0, 'character': 8, 'label': 'end', 'padding_left': True}])
    elif kind == 'shutdown':
        break
    else:
        continue
    print(json.dumps(reply, ensure_ascii=False), flush=True)
''', encoding="utf-8")
        fixture.chmod(0o700)
        self.source_text = ['甲😀value', 'second line']
        e.ex("call setline(1, ['甲😀value', 'second line'])")
        e.ex("set nowrap nolist nospell nonumber norelativenumber nocursorline nocursorcolumn")
        e.ex("highlight SimpleCCInlayHint guifg=#91a2b3 guibg=NONE gui=italic")
        e.ex("let g:simplecc_daemon_path = " + harness.vim_string(fixture)
             + " | setfiletype rust | SimpleCCStart")
        self.wait_requests(lambda requests: any(r['type'] == 'textDocument/didOpen' for r in requests))
        self.source = e.gui_frame()
        self.assertTrue(self.source['supported'], self.source.get('fallback_reason'))
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
        self.fail(f"Expected inlay request was not sent: {self.requests()}")

    @staticmethod
    def labels(frame):
        return ''.join(run['text'] for run in frame['runs'] if run['line'] == 0
                       and run['style']['foreground'] == 0x91a2b3)

    def enable(self):
        self.workbench(self.e, 'simple_inlay_hints')
        return self.e.painted(lambda frame: frame['supported'] and ': 型é' in self.labels(frame))

    def test_named_action_async_paint_utf16_source_bytes_and_toggle_clear(self):
        e = self.start()
        self.assertEqual(self.capture(e, 'g:jterm_vim9_virtual_text_inline'), True)
        frame = self.enable()
        self.assertEqual(frame['changedtick'], self.source['changedtick'])
        self.assertEqual(frame['buffer'], self.source['buffer'])
        self.assertEqual(self.labels(frame), ' : 型é  end')
        hint_runs = [r for r in frame['runs'] if r['line'] == 0
                     and r['style']['foreground'] == 0x91a2b3]
        self.assertTrue(all(r['byte_start'] == r['byte_end'] == 0 for r in hint_runs))
        self.assertTrue(all(r['style']['italic'] for r in hint_runs))
        self.assertTrue(any(r['text'] == '型' and r['cells'] == 2 for r in hint_runs))
        self.assertTrue(any(r['text'] == 'é' and r['cells'] == 1 for r in hint_runs))
        source_runs = [r for r in frame['runs'] if r['line'] == 1 and r['byte_end'] > r['byte_start']]
        self.assertEqual(''.join(r['text'] for r in source_runs), self.source_text[0])
        source_bytes = self.source_text[0].encode('utf-8')
        for run in source_runs:
            self.assertEqual(run['text'].encode('utf-8'), source_bytes[run['byte_start']:run['byte_end']])
        self.assertEqual(self.capture(e, "map(prop_list(1), {_, p -> p.col})"), [8, 13])
        window = next(w for w in frame['windows'] if w['window'] == frame['window'])
        self.assertFalse(window['native_pointer'], 'Vim owns clicks inside inline hint gaps')
        self.assertTrue(window['native_scroll'], 'source-bound line scrolling remains available')
        self.assertGreater(self.capture(e, 'g:test_decorations'), 0)
        e.request('save')
        self.assertEqual(e.file.read_text(encoding='utf-8'), '\n'.join(self.source_text) + '\n')
        # Writing can advance Vim's changedtick when it clears 'modified'.
        # Compare the toggle against that completed write, not the old frame.
        saved = e.gui_frame()

        self.workbench(e, 'simple_inlay_hints')
        cleared = e.painted(lambda f: f['supported'] and f['revision'] > frame['revision']
                            and not self.labels(f))
        self.assertEqual(cleared['changedtick'], saved['changedtick'])
        self.assertEqual(self.capture(e, 'prop_list(1)'), [])
        self.assertEqual(self.capture(e, 'g:simplecc_inlay_hints'), 0)
        self.assertEqual(self.capture(e, "getline(1, '$')"), self.source_text)

    def test_empty_provider_reply_clears_without_edit_and_wrap_recovers(self):
        e = self.start()
        frame = self.enable()
        e.ex('set wrap')
        fallback = e.gui_frame()
        self.assertFalse(fallback['supported'])
        self.assertTrue(self.capture(e, '&wrap'), 'fallback must preserve the user display option')
        e.ex('set nowrap')
        recovered = e.gui_frame()
        self.assertTrue(recovered['supported'], recovered.get('fallback_reason'))
        self.assertEqual(self.labels(recovered), self.labels(frame))

        self.mode.write_text('empty', encoding='utf-8')
        e.ex('call simplecc#OnWinScrolled()')
        cleared = e.painted(lambda f: f['supported'] and f['revision'] > recovered['revision']
                            and not self.labels(f))
        self.assertEqual(cleared['changedtick'], frame['changedtick'])
        self.assertEqual(self.capture(e, 'prop_list(1)'), [])
        e.ex('call simplecc#OnCursorHold()')
        self.assertEqual(self.capture(e, 'prop_list(1)'), [], 'empty reply also clears the restore cache')
        self.assertEqual(self.capture(e, "getline(1, '$')"), self.source_text)


if __name__ == '__main__':
    unittest.main(verbosity=2)
