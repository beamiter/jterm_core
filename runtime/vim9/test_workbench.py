#!/usr/bin/env python3
"""Real Vim contract tests for optional, source-bound workbench providers.

These fixtures test the bridge boundary, not an external LSP/Git server.
The companion test_vimrc_workbench suite exercises the actual user's plugins.
"""

from pathlib import Path
import json
import tempfile
import time
import unittest

from test_bridge import Editor, VIM


PROVIDER = """
let g:workbench_actions = ['simple_files', 'simple_rename', 'simple_workbench', 'definition', 'hover', 'references', 'format']
function! VimrcWorkbenchCapabilities() abort
  return g:workbench_actions
endfunction
function! VimrcWorkbenchAction(action) abort
  call writefile([a:action], 'workbench-actions.marker', 'a')
endfunction
function! VimrcWorkbenchState() abort
  return get(g:, 'workbench_state', {'provider': 'simple', 'native': get(g:, 'jterm_vim9_connected', v:false)})
endfunction
function! VimrcWorkbenchDiagnostics() abort
  return get(g:, 'workbench_diagnostics', {})
endfunction
autocmd User JTermVim9Ready call writefile([string(g:jterm_vim9_connected), string(g:jterm_vim9_render_protocol), string(exists('$JTERM_VIM9_TOKEN'))], 'workbench-ready.marker')
autocmd User JTermVim9Disconnected call writefile([string(g:jterm_vim9_connected), string(g:jterm_vim9_render_protocol)], 'workbench-disconnected.marker', 'a')
"""


@unittest.skipUnless(VIM, "Install Vim 9 for real workbench tests")
class WorkbenchTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="jterm-workbench-")
        self.root = Path(self.temp.name)
        self.editors = []

    def tearDown(self):
        for editor in self.editors:
            editor.close()
        self.temp.cleanup()

    def editor(self, **kwargs):
        result = Editor(self.root, vimrc_extra=PROVIDER, **kwargs)
        self.editors.append(result)
        return result

    @staticmethod
    def source(state):
        return {name: state[name] for name in ["window", "buffer", "changedtick"]}

    def test_authenticated_lifecycle_and_disconnect_are_observable(self):
        e = self.editor(authenticate=False)
        self.assertFalse((self.root / "workbench-ready.marker").exists())
        e.authenticate()
        self.assertEqual((self.root / "workbench-ready.marker").read_text(), "v:true\n1\n0\n")
        state = e.snapshot()
        self.assertIs(state["workbench"]["native"], True)
        self.assertEqual(self.source(state["workbench"]), self.source(state))
        e.connection.close()
        e.connection = None
        deadline = time.monotonic() + 2
        while not (self.root / "workbench-disconnected.marker").exists() and time.monotonic() < deadline:
            e.pump(0.02)
        self.assertEqual((self.root / "workbench-disconnected.marker").read_text(), "v:false\n0\n")
        self.assertIsNone(e.process.poll(), "a lost provider bridge must leave Vim alive")

    def test_capabilities_are_closed_bounded_and_rechecked(self):
        e = self.editor()
        e.ex("let g:workbench_actions = ['eval', 'simple_files', 'simple_files', 'rename', 'hover'] + repeat(['unknown'], 80)")
        state = e.snapshot()
        self.assertNotIn("eval", state["capabilities"])
        self.assertNotIn("rename", state["capabilities"])
        self.assertEqual(state["capabilities"].count("simple_files"), 1)
        e.request("workbench", command="simple_files", **self.source(state))
        self.assertEqual((self.root / "workbench-actions.marker").read_text(), "simple_files\n")
        e.ex("let g:workbench_actions = []")
        e.request("workbench", command="simple_files", ok=False, **self.source(e.snapshot()))
        self.assertEqual((self.root / "workbench-actions.marker").read_text(), "simple_files\n")
        e.ex("delfunction VimrcWorkbenchAction")
        self.assertNotIn("simple_workbench", e.snapshot()["capabilities"])

    def test_context_is_explicitly_projected_and_bounded(self):
        e = self.editor()
        e.ex("let g:workbench_state = {'provider': repeat('中', 100), 'root': repeat('r', 6000), 'native': v:true, 'large_file': v:true, 'git': {'head': repeat('h', 300), 'ahead': 99999999, 'behind': -3, 'added': 'bad', 'changed': 2, 'removed': 1}, 'diagnostics': {'error': 4, 'warning': 2, 'info': {}, 'hint': v:true}, 'remote': {'connected': v:true, 'label': repeat('n', 400), 'root': repeat('s', 6000)}, 'secret': 'must-not-cross'}")
        e.ex("let g:workbench_state.cycle = g:workbench_state | let g:workbench_state.Callback = function('tr')")
        state = e.snapshot()
        context = state["workbench"]
        self.assertEqual(len(context["provider"]), 64)
        self.assertEqual(len(context["root"]), 4096)
        self.assertEqual(len(context["git"]["head"]), 128)
        self.assertEqual(context["git"]["ahead"], 1_000_000)
        self.assertEqual(context["git"]["behind"], 0)
        self.assertEqual(context["git"]["added"], 0)
        self.assertEqual(context["diagnostics"], {"error": 4, "warning": 2, "info": 0, "hint": 0})
        self.assertEqual(len(context["remote"]["label"]), 256)
        self.assertEqual(len(context["remote"]["root"]), 4096)
        self.assertNotIn("must-not-cross", json.dumps(state))
        self.assertNotIn("Callback", context)
        self.assertEqual(self.source(context), self.source(state))

    def test_malformed_or_throwing_state_hooks_do_not_break_editing(self):
        e = self.editor()
        for value in ["[]", "'invalid'", "{'provider': 'simple', 'git': [], 'remote': 3, 'diagnostics': v:true}"]:
            e.ex("let g:workbench_state = " + value)
            state = e.snapshot()
            self.assertIsInstance(state["workbench"], dict)
        e.ex("function! VimrcWorkbenchState() abort\nthrow 'provider loading'\nendfunction")
        self.assertEqual(e.snapshot()["workbench"], {})
        e.keys("Q")
        e.request("save")
        self.assertEqual(e.file.read_text().splitlines()[0], "from-user-vimrc")

    def test_unknown_stale_and_invalid_actions_cannot_reach_provider(self):
        e = self.editor()
        initial = self.source(e.snapshot())
        e.request("workbench", command="simple_files | call writefile(['bad'], 'PWNED')", ok=False, **initial)
        self.assertFalse((self.root / "PWNED").exists())
        e.ex("call setline(1, 'changed while palette was open')")
        result = e.request("workbench", command="simple_files", ok=False, **initial)
        self.assertIn("source changed", result["error"])
        current = self.source(e.snapshot())
        e.request("split", vertical=True)
        result = e.request("workbench", command="simple_files", ok=False, **current)
        self.assertIn("source changed", result["error"])
        current = self.source(e.snapshot())
        for invalid in [True, -1, "4", None]:
            request = {**current, "changedtick": invalid}
            e.request("workbench", command="simple_files", ok=False, **request)
        self.assertFalse((self.root / "workbench-actions.marker").exists())
        e.request("workbench", command="simple_files", **current)
        self.assertEqual((self.root / "workbench-actions.marker").read_text(), "simple_files\n")

    def test_semantic_provider_precedence_preserves_typed_rename(self):
        e = self.editor(plugin=True)
        for action in ["definition", "hover", "references", "format"]:
            e.request(action)
        self.assertEqual((self.root / "workbench-actions.marker").read_text().splitlines(),
                         ["definition", "hover", "references", "format"])
        # SimpleCC's prompt cannot consume Rename{name}. The original provider
        # still receives that exact text, without an ignored rename payload.
        e.request("rename", name="fromNativeDialog")
        self.assertEqual((self.root / "rename.marker").read_text(), "fromNativeDialog\n")
        e.request("workbench", command="simple_rename", **self.source(e.snapshot()))
        self.assertEqual((self.root / "workbench-actions.marker").read_text().splitlines()[-1], "simple_rename")
        e.ex("let g:workbench_actions = []")
        e.request("hover")
        self.assertEqual((self.root / "hover.marker").read_text(), "hover\n")

    def test_action_failure_is_reported_without_losing_bridge(self):
        e = self.editor()
        self.assertTrue(e.gui_frame()['supported'])
        e.ex("function! VimrcWorkbenchAction(action) abort\nthrow 'plugin not ready'\nendfunction")
        result = e.request("workbench", command="simple_files", ok=False, **self.source(e.snapshot()))
        self.assertIn("plugin not ready", result["error"])
        self.assertIsInstance(e.snapshot()["buffers"], list)
        self.assertTrue(e.gui_frame()['supported'], 'failed hooks must restore native painting')

    def test_diagnostic_hook_projects_current_source_and_preserves_user_lists(self):
        e = self.editor()
        e.ex("call setqflist([{'filename': expand('%:p'), 'lnum': 2, 'col': 3, 'text': 'shared', 'type': 'W'}])")
        e.ex("let g:workbench_diagnostics = {'items': [{'line': 2, 'column': 3, 'text': 'shared', 'kind': 'warning'}, {'line': 1, 'column': 4, 'text': 'cached problem', 'kind': 'hint', 'filename': '/unrelated/private-file', 'Callback': function('tr')}], 'total': 2}")
        e.ex("let g:workbench_diagnostics.items[1].cycle = g:workbench_diagnostics")
        state = e.snapshot()
        self.assertEqual(len(state['diagnostics']), 2, 'quickfix/provider duplicates merge')
        row = next(row for row in state['diagnostics'] if row['text'] == 'cached problem')
        self.assertEqual(row, {'filename': str(e.file), 'line': 1, 'column': 4,
                               'text': 'cached problem', 'kind': 'hint', 'column_kind': 'byte'})
        self.assertEqual(state['quickfix']['count'], 1)
        self.assertFalse(state['diagnostics_truncated'])
        self.assertNotIn('private-file', json.dumps(state))
        # Changes to cached rows (including provider removal/reload) are visible
        # without a buffer edit or an explicit location-list publication.
        tick = state['changedtick']
        e.ex("let g:workbench_diagnostics = {}")
        cleared = e.snapshot()
        self.assertEqual(cleared['changedtick'], tick)
        self.assertEqual([row['text'] for row in cleared['diagnostics']], ['shared'])

    def test_diagnostic_hook_is_bounded_and_reports_provider_omissions(self):
        e = self.editor()
        e.ex("let g:workbench_diagnostics = {'items': map(range(300), {_, n -> {'line': n + 1, 'column': 1, 'text': 'cached', 'kind': 'error'}}), 'total': 350}")
        state = e.snapshot()
        self.assertEqual(len(state['diagnostics']), 256)
        self.assertTrue(state['diagnostics_truncated'])
        self.assertEqual(state['diagnostics_omitted'], 94)
        e.ex("let g:workbench_diagnostics.total = 99999999")
        self.assertEqual(e.snapshot()['diagnostics_omitted'], 1_000_000 - 256)
        e.ex("let g:workbench_diagnostics = {'items': [{'line': 1, 'column': 1, 'text': repeat('中', 3000), 'kind': 'error'}]}")
        self.assertEqual(len(e.snapshot()['diagnostics'][0]['text']), 2048)

    def test_invalid_or_throwing_diagnostic_hooks_expire_rows_without_breaking_vim(self):
        e = self.editor()
        e.ex("let g:workbench_diagnostics = {'items': [{'line': 1, 'column': 1, 'text': 'first', 'kind': 'error'}]}")
        self.assertEqual(len(e.snapshot()['diagnostics']), 1)
        for value in ["[]", "{'items': 3}",
                      "{'items': [v:null, [], {'line': v:true, 'column': 1, 'text': 'bad', 'kind': 'error'}, {'line': 1, 'column': 0, 'text': 'bad', 'kind': 'hint'}, {'line': 1, 'column': 1, 'text': {}, 'kind': 'info'}, {'line': 1, 'column': 1, 'text': 'bad', 'kind': 'unknown'}]}"]:
            e.ex('let g:workbench_diagnostics = ' + value)
            state = e.snapshot()
            self.assertEqual(state['diagnostics'], [])
            self.assertEqual(state['diagnostics_omitted'], 0)
        e.ex("function! VimrcWorkbenchDiagnostics() abort\nthrow 'provider reloading'\nendfunction")
        self.assertEqual(e.snapshot()['diagnostics'], [])
        e.ex('delfunction VimrcWorkbenchDiagnostics')
        self.assertEqual(e.snapshot()['diagnostics'], [])
        e.keys('Q')
        e.request('save')
        self.assertEqual(e.file.read_text().splitlines()[0], 'from-user-vimrc')

    def test_blocking_provider_retires_native_overlay_before_waiting_for_keys(self):
        e = self.editor()
        e.ex("function! VimrcWorkbenchAction(action) abort\nlet g:provider_key = getcharstr()\nendfunction")
        self.assertTrue(e.gui_frame()['supported'])
        source = self.source(e.snapshot())
        request_id = e.next_id
        e.next_id += 1
        e.send({'id': request_id, 'action': 'workbench', 'command': 'simple_files', **source})
        frame = e.wait(lambda m: m.get('event') == 'render' and not m.get('supported', False))
        self.assertIn('workbench', frame['fallback_reason'])
        refused = e.request('open', path=str(e.other), ok=False)
        self.assertIn('waiting for input', refused['error'])
        e.request('save', ok=False)
        e.keys(b'x')
        self.assertTrue(e.wait(lambda m: m.get('event') == 'response' and m.get('id') == request_id)['ok'])
        restored = e.wait(lambda m: m.get('event') == 'render' and m.get('supported')
                          and m['revision'] > frame['revision'])
        self.assertEqual(restored['buffer'], source['buffer'])
        self.assertEqual(e.snapshot()['changedtick'], source['changedtick'])

    def test_exit_notifies_once_even_if_user_lifecycle_hook_throws(self):
        e = self.editor()
        e.ex("autocmd User JTermVim9Disconnected throw 'optional shutdown observer failed'")
        e.request("quit")
        e.process.wait(timeout=3)
        self.assertEqual((self.root / "workbench-disconnected.marker").read_text(), "v:false\n0\n")


if __name__ == "__main__":
    unittest.main(verbosity=2)
