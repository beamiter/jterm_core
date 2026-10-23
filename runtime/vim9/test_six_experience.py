#!/usr/bin/env python3
"""Real six-feature integration: typed native actions, Vimrc and SimpleCC.

Use VIM9_TEST_VIMRC and VIM9_TEST_SIMPLE_ROOT as test_vimrc_workbench does.
Only the external daemon's JSON replies are synthetic; Vim commands, document
edits, split layout, save policy and native rendering are the shipped code.
"""
from __future__ import annotations

import json
import time
import unittest

import test_vimrc_workbench as harness
from test_render_popups import PopupEditor


class SixEditor(PopupEditor):
    render_protocol = 7


@unittest.skipUnless(harness.AVAILABLE, "Needs Vim, Vimrc and simple* checkouts")
class SixExperienceTests(unittest.TestCase):
    setUp = harness.VimrcWorkbenchTests.setUp
    tearDown = harness.VimrcWorkbenchTests.tearDown
    editor = harness.VimrcWorkbenchTests.editor
    capture = harness.VimrcWorkbenchTests.capture
    workbench = harness.VimrcWorkbenchTests.workbench

    def requests(self):
        log = self.root / "six.requests"
        return [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []

    def until(self, editor, predicate):
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            if predicate():
                return
            editor.pump(.03)
        self.fail("Expected real Vim/daemon result; requests=" + repr(self.requests()))

    def start(self, *, daemon=False):
        setup = ""
        if daemon:
            peer = self.root / "six-daemon"
            peer.write_text(r'''#!/usr/bin/env python3
import json
from pathlib import Path
import sys
root = Path(__file__).parent
for line in sys.stdin:
    request = json.loads(line)
    with (root / 'six.requests').open('a') as log:
        log.write(json.dumps(request) + '\n')
    kind = request['type']
    reply = {'id': request.get('id', 0)}
    if kind == 'initialize':
        reply.update(type='initialized', capabilities={
            'code_action_identity': True, 'save_pipeline': True, 'file_rename': True})
    elif kind == 'shutdown':
        break
    elif kind == 'textDocument/signatureHelp':
        label = '函数(参数😀, 参数😀)'
        start = len(label[:label.rfind('参数😀')].encode('utf-8'))
        reply.update(type='signatureHelp', signatures=[{
            'label': label, 'active_parameter': 1,
            'overload_index': 2, 'overload_count': 4,
            'parameters': [{'label': '参数😀'}, {'label': '参数😀',
                'byte_range': [start, start + len('参数😀'.encode('utf-8'))]}],
            'documentation': 'Example\n```python\ndef greeting():\n    return "hello"\n```'}])
    elif kind in ('textDocument/formatting', 'textDocument/organizeImports'):
        if (root / 'save.fail').exists():
            reply.update(type='error', message='fixture formatter refused unsafe changes')
        else:
            imports = kind.endswith('organizeImports')
            reply.update(type=kind.split('/')[-1], edits=[{
                'line': 0, 'character': 0, 'end_line': 0,
                'end_character': 9 if imports else 3,
                'new_text': 'organized' if imports else 'formatted'}])
    else:
        continue
    print(json.dumps(reply, ensure_ascii=False), flush=True)
''', encoding="utf-8")
            peer.chmod(0o700)
            setup = (
                "let g:simplecc_daemon_path = " + harness.vim_string(peer) + "\n"
                "let g:simplecc_signature_help = 0\n"
                "let g:simplecc_diag_float = 0\n"
                "autocmd VimEnter * setfiletype rust | call g:VimrcConfigureFiletype() | SimpleCCStart\n"
            )
        editor = self.editor(editor_class=SixEditor, extra_setup=setup)
        if daemon:
            self.until(editor, lambda: self.capture(editor, "get(g:, 'simplecc_status', '')") == 'ready')
        self.assertTrue(editor.gui_frame()["supported"])
        return editor

    def test_typed_close_preserves_split_layout_and_reopens_position(self):
        e = self.start()
        e.file.write_text("first line\nsecond line\nthird line\n", encoding="utf-8")
        e.ex("edit! | split | vsplit | call cursor(2, 4)")
        before = self.capture(e, "{'layout': winlayout(), 'ids': map(getwininfo(), {_, w -> w.winid}), 'window': win_getid()}")
        e.request("close_current_buffer")
        after = self.capture(e, "{'layout': winlayout(), 'ids': map(getwininfo(), {_, w -> w.winid}), 'window': win_getid()}")
        self.assertEqual(after, before, "Closing a buffer must preserve every split/window identity")
        e.request("reopen_buffer")
        restored = e.snapshot()
        self.assertEqual((restored["file"], restored["line"], restored["column"]), (str(e.file), 2, 4))
        e.keys("iUNSAVED\x1b")
        e.state_where(lambda state: state["modified"] and state["mode"] == "n")
        refused = e.request("close_current_buffer", ok=False)
        self.assertIn("Unsaved", refused.get("error", ""))
        self.assertEqual(self.capture(e, "winlayout()"), before["layout"])
        self.assertTrue(e.snapshot()["modified"])
        self.assertEqual(e.file.read_text(), "first line\nsecond line\nthird line\n")

    def test_signature_native_span_overload_and_workbench_pin(self):
        e = self.start(daemon=True)
        e.ex("highlight SimpleCCActiveParameter guifg=#ed12ab guibg=#123456 gui=bold")
        origin = e.snapshot()
        e.ex("SimpleCCSignatureHelp")
        frame = e.painted(lambda paint: any(p.get("kind") == "signature" for p in paint.get("popups", [])))
        popup = next(p for p in frame["popups"] if p.get("kind") == "signature")
        text = "".join(run["text"] for run in popup["runs"])
        self.assertIn("Overload 3 / 4", text)
        highlighted = "".join(run["text"] for run in popup["runs"]
                              if run["style"]["foreground"] == 0xed12ab)
        self.assertEqual(highlighted, "参数😀", "Only the selected repeated parameter may be highlighted")
        self.workbench(e, "simple_doc_pin")
        pinned = self.capture(e, "{'type': &buftype, 'readonly': &readonly, 'modifiable': &modifiable, 'lines': getline(1, '$'), 'origin': b:simplecc_doc_origin, 'found': search('greeting', 'nw')}")
        self.assertEqual((pinned["type"], pinned["readonly"], pinned["modifiable"]), ("nofile", 1, 0))
        self.assertEqual(pinned["origin"]["bufnr"], origin["buffer"])
        self.assertEqual(pinned["lines"][:2], ["函数(参数😀, 参数😀)", "Overload 3 / 4"])
        self.assertGreater(pinned["found"], 0)
        e.keys("gg0y$")
        self.assertEqual(self.capture(e, "getreg('\"')"), pinned["lines"][0])
        self.assertFalse(e.gui_frame().get("popups"))
        self.assertEqual(self.capture(e, "getbufline(" + str(origin["buffer"]) + ", 1, '$')"), ["original line", "second line"])

    def prepare_save(self):
        e = self.start(daemon=True)
        e.ex("call setline(1, 'raw') | let b:vimrc_save_pipeline = {'format': v:true, 'imports': v:true}")
        return e

    def test_native_save_uses_optin_format_imports_then_writes(self):
        e = self.prepare_save()
        e.request("save")
        self.until(e, lambda: e.file.read_text().startswith("organized\n"))
        self.assertFalse(e.snapshot()["modified"])
        messages = self.requests()
        kinds = [request["type"] for request in messages]
        formatted = kinds.index("textDocument/formatting")
        imports = kinds.index("textDocument/organizeImports")
        self.assertLess(formatted, imports)
        self.assertIn("textDocument/didChange", kinds[formatted + 1:imports], "Format edits must reach LSP before imports")
        self.assertEqual(e.file.read_text(), "organized\nsecond line\n")
        e.keys("u")
        self.until(e, lambda: self.capture(e, "getline(1)") == "raw")
        self.assertTrue(e.snapshot()["modified"], "One undo must retain the user's pre-save input")

    def test_native_build_cannot_bypass_failed_save_pipeline(self):
        e = self.prepare_save()
        (self.root / "save.fail").touch()
        failed = e.request("save_and_build", command="touch SHOULD_NOT_BUILD", ok=False)
        self.assertIn("formatter", failed.get("error", ""))
        self.assertFalse((self.root / "SHOULD_NOT_BUILD").exists())
        self.assertEqual(e.file.read_text(), "original line\nsecond line\n")
        self.assertTrue(e.snapshot()["modified"])
        kinds = [request["type"] for request in self.requests()]
        self.assertIn("textDocument/formatting", kinds)
        self.assertNotIn("textDocument/organizeImports", kinds)
        self.assertFalse(e.snapshot()["build"]["running"])
        (self.root / "save.fail").unlink()
        e.request("save_and_build", command="cat main.txt > built.txt")
        self.until(e, lambda: (self.root / "built.txt").exists())
        self.assertEqual((self.root / "built.txt").read_text(), "organized\nsecond line\n")


if __name__ == "__main__":
    unittest.main()
