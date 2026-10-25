#!/usr/bin/env python3
"""Real source-bound diagnostic, symbol and existing-terminal workflows."""
from __future__ import annotations

import json
from pathlib import Path
import time
import unittest

import test_vimrc_workbench as workbench

PLUGINS = workbench.PLUGINS + ("simpleterminal",)
AVAILABLE = workbench.AVAILABLE and (workbench.PLUGIN_ROOT / "simpleterminal/plugin/simpleterminal.vim").is_file()


@unittest.skipUnless(AVAILABLE, "Needs Vim and the actual Simple plugin checkouts")
class WorkbenchWorkflowTests(unittest.TestCase):
    setUp = workbench.VimrcWorkbenchTests.setUp
    tearDown = workbench.VimrcWorkbenchTests.tearDown
    capture = workbench.VimrcWorkbenchTests.capture
    workbench = workbench.VimrcWorkbenchTests.workbench

    def editor(self, extra_setup=""):
        return workbench.VimrcWorkbenchTests.editor(self, plugins=PLUGINS, extra_setup=extra_setup)

    def until(self, editor, predicate):
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            if predicate():
                return
            editor.pump(.025)
        self.fail("The actual plugin did not finish the requested workflow")

    def semantic_editor(self, *, supported=True):
        fixture = self.root / "workflow-daemon"
        fixture.write_text('''#!/usr/bin/env python3
import json, sys
from pathlib import Path
for line in sys.stdin:
 r=json.loads(line)
 with Path(__file__).with_suffix('.requests').open('a') as f:f.write(json.dumps(r)+'\\n')
 t=r['type']; reply={'id':r.get('id',0)}
 if t=='initialize':reply.update(type='initialized')
 elif t=='textDocument/didOpen':
  print(json.dumps({'type':'serverStatus','server':'fixture','status':'running','filetypes':['rust'],'capabilities':{'document_highlight':SUPPORTED,'diagnostics':SUPPORTED}}),flush=True)
  reply={'type':'diagnostics','uri':r['uri'],'items':[
   {'line':0,'character':0,'end_line':0,'end_character':1,'severity':1,'message':'first error'},
   {'line':1,'character':1,'end_line':1,'end_character':2,'severity':2,'message':'skip this warning'},
   {'line':1,'character':6,'end_line':1,'end_character':7,'severity':1,'message':'second error'}]}
 elif t=='textDocument/pullDiagnostics':reply.update(type='diagnostics',uri=r['uri'],items=[])
 elif t=='textDocument/documentHighlight':reply.update(type='documentHighlight',highlights=[{'line':0,'character':0,'end_line':0,'end_character':8,'kind':'read'}])
 elif t=='shutdown':break
 else:continue
 print(json.dumps(reply),flush=True)
'''.replace('SUPPORTED', 'True' if supported else 'False'), encoding='utf-8')
        fixture.chmod(0o700)
        e = self.editor("let g:simplecc_daemon_path = " + workbench.vim_string(fixture)
                        + "\nautocmd VimEnter * setfiletype rust | SimpleCCStart\n")
        e.state_where(lambda s: s.get('workbench', {}).get('diagnostics', {}).get('error') == 2)
        return e

    def test_error_navigation_filters_warnings_and_refresh_expires_cached_errors(self):
        e = self.semantic_editor()
        source = e.snapshot()
        self.assertIn('simple_next_error', source['capabilities'])
        self.workbench(e, 'simple_next_error')
        self.assertEqual((e.snapshot()['line'], e.snapshot()['column']), (2, 7))
        self.workbench(e, 'simple_previous_error')
        self.assertEqual((e.snapshot()['line'], e.snapshot()['column']), (1, 1))
        self.workbench(e, 'simple_diagnostics_errors')
        self.assertEqual(self.capture(e, "map(getloclist(0), {_, item -> item.type})"), ['E', 'E'])
        e.ex('lclose')
        self.workbench(e, 'simple_refresh_diagnostics')
        empty = e.state_where(lambda s: s.get('workbench', {}).get('diagnostics', {}).get('error') == 0)
        self.assertNotIn('simple_next_error', empty['capabilities'])
        self.workbench(e, 'simple_next_error', ok=False)
        self.assertEqual(self.capture(e, "getline(1, '$')"), ['original line', 'second line'])
        self.assertFalse(e.snapshot()['modified'])

    def test_symbol_highlight_clear_is_source_bound_and_does_not_edit(self):
        e = self.semantic_editor()
        before = e.snapshot()
        self.workbench(e, 'simple_highlight_symbol')
        self.until(e, lambda: bool(self.capture(e, "prop_list(1, {'types': ['SimpleCCDocHighlightRead']})")))
        self.assertIn('simple_clear_highlights', e.snapshot()['capabilities'])
        self.workbench(e, 'simple_clear_highlights')
        self.assertEqual(self.capture(e, "prop_list(1, {'types': ['SimpleCCDocHighlightRead']})"), [])
        self.assertNotIn('simple_clear_highlights', e.snapshot()['capabilities'])
        e.keys('j')
        e.state_where(lambda s: s.get('line') == 2)
        self.workbench(e, 'simple_highlight_symbol', state=before, ok=False)
        self.assertFalse(e.snapshot()['modified'])
        e.ex('setlocal filetype=markdown')
        self.assertNotIn('simple_highlight_symbol', e.snapshot()['capabilities'])
        self.assertFalse(self.capture(e, "simplecc#WorkbenchSupports('document_highlight')"))

    def test_server_feature_and_filetype_checks_do_not_advertise_dead_actions(self):
        e = self.semantic_editor(supported=False)
        for action in ('simple_highlight_symbol', 'simple_refresh_diagnostics'):
            self.assertNotIn(action, e.snapshot()['capabilities'])
            self.workbench(e, action, ok=False)
        # A running Rust service must not claim another filetype's support.
        self.assertFalse(self.capture(e, "simplecc#WorkbenchSupports('unknown')"))
        e.ex('setlocal filetype=markdown')
        self.assertFalse(self.capture(e, "simplecc#WorkbenchSupports('diagnostics')"))

    def terminal_editor(self):
        (self.root / '.git').mkdir()
        receiver = self.root / 'receiver.py'
        receiver.write_text("import sys\nfrom pathlib import Path\nfor line in sys.stdin:\n with Path(sys.argv[1]).open('a') as f:f.write(line)\n")
        self.received = self.root / 'received.txt'
        spec = {'command': ['python3', '-u', str(receiver), str(self.received)],
                'cwd': str(self.root), 'name': 'fixture receiver', 'remote': False}
        setup = "\n".join([
            "let g:test_terminal_spec = json_decode(" + workbench.vim_string(json.dumps(spec)) + ")",
            "let g:SimpleTerminalSpecProvider = {argument -> copy(g:test_terminal_spec)}",
        ])
        return self.editor(setup)

    def received_lines(self):
        return self.received.read_text().splitlines() if self.received.exists() else []

    def test_terminal_line_uses_existing_owned_workspace_and_rejects_stale_source(self):
        e = self.terminal_editor()
        self.assertNotIn('simple_terminal_send_line', e.snapshot()['capabilities'])
        jobs = self.capture(e, 'len(job_info())')
        for _ in range(3):
            e.snapshot()
        self.assertEqual(self.capture(e, 'len(job_info())'), jobs)
        # Metadata reads do not create a terminal; this is an explicit setup.
        e.ex('call simpleterminal#New('') | call simpleterminal#Hide()')
        text = "literal|$HOME; 中文"
        e.ex("call setline(1, " + workbench.vim_string(text) + ")")
        self.until(e, lambda: 'simple_terminal_send_line' in e.snapshot()['capabilities'])
        source = e.snapshot()
        self.workbench(e, 'simple_terminal_send_line')
        self.until(e, lambda: self.received_lines() == [text])
        e.keys('j')
        e.state_where(lambda s: s.get('line') == 2)
        self.workbench(e, 'simple_terminal_send_line', state=source, ok=False)
        self.assertEqual(self.received_lines(), [text])
        other = self.root / 'other-project'
        other.mkdir()
        e.ex("let g:test_terminal_spec.cwd = " + workbench.vim_string(other)
             + " | call simpleterminal#New('') | call simpleterminal#Hide()")
        self.assertNotIn('simple_terminal_send_line', e.snapshot()['capabilities'])
        self.workbench(e, 'simple_terminal_send_line', ok=False)
        self.assertEqual(self.received_lines(), [text])
        self.assertEqual(e.file.read_text(), 'original line\nsecond line\n')
        e.ex('SimpleTerminalKill')
        e.ex('SimpleTerminalKill')

    def test_exited_current_terminal_never_redirects_to_another_live_session(self):
        e = self.terminal_editor()
        e.ex("call simpleterminal#New('') | call simpleterminal#Hide()")
        e.ex("let g:test_terminal_spec.command = ['sh', '-c', 'sleep 0.2']"
             + " | call simpleterminal#New('') | call simpleterminal#Hide()")
        self.until(e, lambda: 'simple_terminal_send_line' not in e.snapshot()['capabilities'])
        self.workbench(e, 'simple_terminal_send_line', ok=False)
        e.ex("let g:test_delivery_error = '' | try | call simpleterminal#SendWorkbenchLine("
             + workbench.vim_string(self.root)
             + ", {}, 'do-not-retarget') | catch | let g:test_delivery_error = v:exception | endtry")
        self.assertIn('no live current terminal', self.capture(e, 'g:test_delivery_error'))
        self.assertEqual(self.received_lines(), [])
        e.ex('SimpleTerminalKill')
        e.ex('SimpleTerminalKill')

    def test_terminal_source_control_keys_are_rejected_and_tab_indent_is_preserved(self):
        e = self.terminal_editor()
        e.ex("call simpleterminal#New('') | call simpleterminal#Hide()")
        for code in (3, 8, 10, 13, 27, 127):
            e.ex("call setline(1, nr2char(" + str(code) + ") .. 'control-key')")
            self.assertNotIn('simple_terminal_send_line', e.snapshot()['capabilities'])
            self.workbench(e, 'simple_terminal_send_line', ok=False)
            e.ex("let g:test_delivery_error = '' | try | call simpleterminal#SendWorkbenchLine("
                 + workbench.vim_string(self.root) + ", {}, nr2char(" + str(code)
                 + ") .. 'control-key') | catch | let g:test_delivery_error = v:exception | endtry")
            self.assertIn('control keys', self.capture(e, 'g:test_delivery_error'))
        self.assertEqual(self.received_lines(), [])
        e.ex("call setline(1, \"\\tindented-code\")")
        self.until(e, lambda: 'simple_terminal_send_line' in e.snapshot()['capabilities'])
        self.workbench(e, 'simple_terminal_send_line')
        self.until(e, lambda: self.received_lines() == ['\tindented-code'])
        e.ex('SimpleTerminalKill')

    def test_remote_source_generation_must_match_current_workspace_and_terminal(self):
        e = self.terminal_editor()
        workspace = {'id': 8, 'kind': 'ssh', 'target': 'host-B', 'root': '/srv/project'}
        encoded = workbench.vim_string(json.dumps(workspace))
        e.ex("let g:simpleremote_workspace = json_decode(" + encoded + ")"
             + " | let g:simpleremote_status = 'ssh:host-B'"
             + " | let g:test_terminal_spec.remote = v:true"
             + " | let g:test_terminal_spec.workspace = copy(g:simpleremote_workspace)"
             + " | call simpleterminal#New('') | call simpleterminal#Hide()")
        e.ex("silent enew | silent file remote:///srv/project/code.rs | setlocal buftype=acwrite filetype=rust"
             + " | call setline(1, 'remote-line')"
             + " | let b:vimrc_remote = {'path':'/srv/project/code.rs', 'uri':'remote:///srv/project/code.rs', 'generation':7}")
        self.assertNotIn('simple_terminal_send_line', e.snapshot()['capabilities'])
        self.workbench(e, 'simple_terminal_send_line', ok=False)
        self.assertEqual(self.received_lines(), [])
        e.ex('let b:vimrc_remote.generation = 8')
        self.until(e, lambda: 'simple_terminal_send_line' in e.snapshot()['capabilities'])
        self.workbench(e, 'simple_terminal_send_line')
        self.until(e, lambda: self.received_lines() == ['remote-line'])
        e.ex("let b:vimrc_remote.path = '/other/project/code.rs'")
        self.assertNotIn('simple_terminal_send_line', e.snapshot()['capabilities'])
        self.workbench(e, 'simple_terminal_send_line', ok=False)
        e.ex('SimpleTerminalKill')


if __name__ == '__main__':
    unittest.main(verbosity=2)
