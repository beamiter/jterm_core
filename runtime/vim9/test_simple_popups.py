#!/usr/bin/env python3
"""Optional real SimpleCC/SimpleGit popup producers against Vim protocol 6.

Use sibling checkouts, or VIM9_SIMPLECC_ROOT / VIM9_SIMPLEGIT_ROOT. SimpleCC
callbacks receive local fixtures without starting a language server. SimpleGit
uses its checked-in Python protocol fixture, never an installed daemon or Git
service. No callback, popup option, registration marker or refresh event from
either provider is replaced by the test.
"""

import json
import os
from pathlib import Path
import shutil
import tempfile
import unittest

from test_bridge import VIM
from test_render_popups import PopupEditor, vim_json


CHECKOUT_PARENT = Path(__file__).resolve().parents[3]
SIMPLECC = Path(os.environ.get("VIM9_SIMPLECC_ROOT", CHECKOUT_PARENT / "simplecc")).resolve()
SIMPLEGIT = Path(os.environ.get("VIM9_SIMPLEGIT_ROOT", CHECKOUT_PARENT / "simplegit")).resolve()
HAS_SIMPLECC = (SIMPLECC / "autoload/simplecc.vim").is_file()
HAS_SIMPLEGIT = (SIMPLEGIT / "tests/simplegit_fake_daemon.py").is_file()


@unittest.skipUnless(VIM, "Install Vim 9 for actual provider popup tests")
class SimplePopupTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-simple-popups-")
        self.root = Path(self.directory.name)
        self.editor = None

    def tearDown(self):
        if self.editor is not None:
            self.editor.close()
        self.directory.cleanup()

    def start(self, provider, *, extra_env=None, extra_config=""):
        name = "simplecc" if provider == SIMPLECC else "simplegit"
        # Disable ambient jobs/installers; only explicit fixture requests below
        # may start the checked-in Python SimpleGit protocol stub.
        options = {
            "simplecc_auto_start": 0, "simplecc_auto_complete": 0,
            "simplecc_auto_restart": 0, "simplecc_auto_install": 0,
            "simplecc_remote_auto_restart": 0, "simplecc_no_default_maps": 1,
            "simplecc_inlay_hints": 0, "simplecc_signature_help": 0,
            "simplecc_virtual_diag": 0, "simplecc_diag_float": 0,
            "simplecc_semantic_tokens": 0, "simplegit_auto_enable": 0,
            "simplegit_line_blame": 0, "simplegit_watch": 0,
            "simplegit_signs": 0, "simplegit_enable_default_mappings": 0,
        }
        config = "".join(f"let g:{key} = {value}\n" for key, value in options.items())
        config += extra_config
        config += (
            "let g:test_popup_events = []\n"
            "function! RecordProviderPopup()\n"
            "  let surfaces = []\n"
            "  for id in popup_list()\n"
            "    call add(surfaces, {'id':id, 'marker':getwinvar(id, 'jterm_native_popup', 0), 'filetype':getbufvar(winbufnr(id), '&filetype')})\n"
            "  endfor\n"
            "  call add(g:test_popup_events, surfaces)\n"
            "endfunction\n"
            "augroup TestProviderPopup\n"
            "  autocmd!\n"
            "  autocmd User JTermVim9PopupChanged call RecordProviderPopup()\n"
            "augroup END\n"
            f"execute 'set runtimepath^=' . fnameescape({vim_json(str(provider))})\n"
            f"runtime plugin/{name}.vim\n"
            f"runtime autoload/{name}.vim\n"
        )
        self.editor = PopupEditor(self.root, vimrc_extra=config, extra_env=extra_env)
        e = self.editor
        e.ex("set nonumber norelativenumber nocursorline nocursorcolumn nowrap noshowmode")
        e.ex("highlight Normal guifg=#eeeeee guibg=#20242c gui=NONE")
        e.ex("highlight Pmenu guifg=#c4ced8 guibg=#334455 gui=NONE")
        e.ex("highlight SimpleCCFloatBorder guifg=#bbaa77 guibg=#20242c gui=NONE")
        e.ex("call setline(1, map(range(1, 40), {_,n -> 'source line ' . n})) | normal! 10G")
        self.source_lines = self.read_json("getline(1, '$')")
        self.source = e.gui_frame()
        self.assertTrue(self.source["supported"], self.source.get("fallback_reason"))
        script = str((provider / f"autoload/{name}.vim").resolve())
        sid = self.read_json(f"filter(getscriptinfo(), {{_,s -> s.name ==# {vim_json(script)}}})[0].sid")
        self.prefix = f"<SNR>{sid}_"
        return e

    def read_json(self, expression):
        self.editor.ex("call writefile([json_encode(" + expression + ")], 'oracle.json')")
        return json.loads((self.root / "oracle.json").read_text(encoding="utf-8"))

    def callback(self, name, *arguments):
        kind = {"OnHover": "hover", "OnSignatureHelp": "signature"}.get(name)
        if kind:
            # Current providers require the real request anchor before accepting
            # a reply. Older pinned providers accept the extra id unchanged.
            self.popup_sequence = getattr(self, "popup_sequence", 3000) + 1
            arguments = (dict(arguments[0], id=self.popup_sequence), *arguments[1:])
            self.editor.ex(
                f"if exists('*{self.prefix}RememberPopupRequest') | "
                f"call {self.prefix}RememberPopupRequest('{kind}', {self.popup_sequence}) | endif")
        self.editor.ex(f"call call({vim_json(self.prefix + name)}, {vim_json(arguments)})")

    def stop_watchdog(self):
        # Async provider changes must repaint through their own event, without
        # eventually being discovered by the bridge's 750ms fallback poll.
        self.editor.ex("let g:test_stopped_polls = 0 | for t in timer_info() | if t.time == 750 && string(t.callback) =~# '_Poll' | call timer_stop(t.id) | let g:test_stopped_polls += 1 | endif | endfor")
        self.assertEqual(self.read_json("g:test_stopped_polls"), 1)

    def native_popup(self, frame):
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        self.assertEqual(len(frame["popups"]), 1)
        self.assertEqual(frame["window"], self.source["window"])
        self.assertEqual(frame["buffer"], self.source["buffer"])
        popup = frame["popups"][0]
        self.assertTrue(popup["runs"])
        for run in popup["runs"]:
            self.assertEqual((run["window"], run["line"], run["byte_start"], run["byte_end"]), (0, 0, 0, 0))
        return popup

    def assert_source_unchanged(self):
        self.assertEqual(self.read_json("getline(1, '$')"), self.source_lines)
        state = self.editor.snapshot()
        self.assertEqual(state["window"], self.source["window"])
        self.assertEqual(state["buffer"], self.source["buffer"])
        self.assertEqual(state["changedtick"], self.source["changedtick"])

    def assert_event_marker(self, popup, filetype=None):
        events = self.read_json("g:test_popup_events")
        self.assertTrue(events, "The actual provider must emit the refresh event")
        registered = next(item for item in events[-1] if item["id"] == popup["id"])
        self.assertEqual(registered["marker"], 1)
        if filetype is not None:
            self.assertEqual(registered["filetype"], filetype,
                             "Register after filetype/syntax setup, before the native repaint")

    @unittest.skipUnless(HAS_SIMPLECC, "Set VIM9_SIMPLECC_ROOT to a SimpleCC checkout")
    def test_simplecc_actual_hover_and_signature_callbacks(self):
        e = self.start(SIMPLECC)
        self.callback("OnHover", {"contents": "**alpha**\nHover 界é documentation"})
        popup = self.native_popup(e.gui_frame())
        text = "".join(run["text"] for run in popup["runs"])
        self.assertIn("Hover 界é documentation", text)
        self.assert_event_marker(popup, "markdown")
        self.assert_source_unchanged()
        e.keys("j")
        e.painted(lambda frame: frame["supported"] and frame.get("popups") == [])

        self.callback("OnSignatureHelp", {"signatures": [{
            "label": "alpha(value: str)", "active_parameter": 0,
            "parameters": [{"label": "value", "documentation": "input 界"}],
            "documentation": "Returns the formatted value.",
        }]})
        popup = self.native_popup(e.gui_frame())
        text = "".join(run["text"] for run in popup["runs"])
        self.assertIn("alpha(value: str)", text)
        self.assertIn("value: input 界", text)
        self.assert_event_marker(popup)
        self.assert_source_unchanged()
        self.stop_watchdog()
        script = self.root / "close-signature.vim"
        script.write_text(
            f"let g:ProviderSignature = function('{self.prefix}OnSignatureHelp')\n"
            "function! CloseProviderSignature(timer)\n"
            f"  if exists('*{self.prefix}RememberPopupRequest')\n"
            f"    call {self.prefix}RememberPopupRequest('signature', 4000)\n"
            "  endif\n"
            "  call g:ProviderSignature({'id':4000, 'signatures':[]})\n"
            "  call writefile([json_encode({'events':g:test_popup_events, 'popups':popup_list()})], 'signature-closed.json')\n"
            "endfunction\n", encoding="utf-8")
        e.ex("source " + str(script))
        before = e.gui_frame()
        self.native_popup(before)
        e.ex("call timer_start(100, 'CloseProviderSignature')")
        closed = e.painted(lambda f: f["revision"] > before["revision"] and f["supported"] and f.get("popups") == [])
        self.assertEqual(closed["changedtick"], before["changedtick"])
        recorded = json.loads((self.root / "signature-closed.json").read_text())
        self.assertEqual(recorded["popups"], [])
        self.assertEqual(recorded["events"][-1], [], "Closing alone must notify the native host")
        self.assert_source_unchanged()

    @unittest.skipUnless(HAS_SIMPLECC, "Set VIM9_SIMPLECC_ROOT to a SimpleCC checkout")
    def test_simplecc_actual_diagnostic_publish_and_public_detail_command(self):
        e = self.start(SIMPLECC)
        self.callback("OnDiagnostics", {"uri": e.file.as_uri(), "server": "fixture", "items": [{
            "line": 9, "character": 0, "end_line": 9, "end_character": 6,
            "severity": 1, "source": "fixture", "code": "E123",
            "message": "Unknown name 界é\nDefine it before use.",
        }]})
        e.ex("SimpleCCDiag")
        popup = self.native_popup(e.gui_frame())
        text = "".join(run["text"] for run in popup["runs"])
        self.assertIn("[Error fixture(E123)] Unknown name 界é", text)
        self.assertIn("Define it before use.", text)
        self.assert_event_marker(popup)
        self.assertEqual(popup["default_style"]["background"], 0x20242c)
        self.assert_source_unchanged()
        e.keys("j")
        e.painted(lambda frame: frame["supported"] and frame.get("popups") == [])

    @unittest.skipUnless(HAS_SIMPLECC, "Set VIM9_SIMPLECC_ROOT to a SimpleCC checkout")
    def test_simplecc_actual_async_documentation_during_completion(self):
        e = self.start(SIMPLECC)
        script = self.root / "provider-docs.vim"
        script.write_text(
            f"let g:ProviderDocs = function('{self.prefix}ShowCompletionDocumentation')\n"
            "function! ProviderDocsTimer(timer)\n"
            "  if !pumvisible() | return | endif\n"
            "  call g:ProviderDocs({'detail':'alpha(value: str)', 'documentation':'Async documentation 界é'})\n"
            "  call writefile([json_encode({'events':g:test_popup_events, 'pum':pumvisible(), 'window':win_getid(), 'buffer':bufnr('%')})], 'docs-created.json')\n"
            "endfunction\n"
            "autocmd CompleteDone * call g:ProviderDocs({})\n",
            encoding="utf-8")
        e.ex("source " + str(script))
        e.ex("set completeopt=menuone,noinsert,noselect | call setline(10, '')")
        e.ex("highlight! link PmenuMatch Pmenu | highlight! link PmenuMatchSel PmenuSel")
        self.stop_watchdog()
        before = e.gui_frame()
        e.keys("A\x12=[complete(col('.'), ['alpha', 'beta']), timer_start(100, 'ProviderDocsTimer'), ''][2]\r")
        # No set_render/requested keyframe here: the real provider's event
        # must schedule the asynchronous native frame while Insert mode waits.
        frame = e.painted(lambda f: f["revision"] > before["revision"] and f.get("completion") is not None and bool(f.get("popups")))
        popup = self.native_popup(frame)
        self.assertIn("Async documentation 界é", "".join(run["text"] for run in popup["runs"]))
        created = json.loads((self.root / "docs-created.json").read_text(encoding="utf-8"))
        self.assertEqual(created["pum"], 1)
        self.assertEqual(created["events"][-1][0]["marker"], 1)
        self.assertEqual((created["window"], created["buffer"]), (before["window"], before["buffer"]))
        e.keys("\x0e\x19\x1b")
        e.painted(lambda f: f["mode"] == "n" and f.get("completion") is None and f.get("popups") == [])
        self.assertEqual(self.read_json("getline(10)"), "alpha")
        lines = self.read_json("getline(1, '$')")
        self.assertEqual(lines[:9] + lines[10:], self.source_lines[:9] + self.source_lines[10:])

    @unittest.skipUnless(HAS_SIMPLEGIT, "Set VIM9_SIMPLEGIT_ROOT to a SimpleGit checkout")
    def test_simplegit_actual_blame_popup_and_public_hunk_preview(self):
        daemon = self.root / "simplegit-test-daemon"
        shutil.copyfile(SIMPLEGIT / "tests/simplegit_fake_daemon.py", daemon)
        daemon.chmod(0o700)
        (self.root / ".git").mkdir()
        log = self.root / "git-requests.jsonl"
        env = {"SIMPLEGIT_FAKE_LOG": str(log), "SIMPLEGIT_FAKE_HUNKS_DELAY_MS": "100",
               "SIMPLEGIT_FAKE_HUNKS": json.dumps([{
                   "old_start": 10, "old_count": 1, "new_start": 10, "new_count": 1,
                   "lines": ["@@ -10 +10 @@", "-old", "+new 界"],
               }])}
        e = self.start(SIMPLEGIT, extra_env=env,
                       extra_config=f"let g:simplegit_daemon_path = {vim_json(str(daemon))}\n")
        self.callback("ShowPopup", ["Commit deadbeef", "Author Alice", "Blame detail 界é"])
        popup = self.native_popup(e.gui_frame())
        self.assertIn("Blame detail 界é", "".join(run["text"] for run in popup["runs"]))
        self.assert_event_marker(popup)
        self.assert_source_unchanged()
        e.ex("call popup_clear() | syntax enable | highlight diffAdded guifg=#45d279 guibg=NONE gui=NONE")
        e.ex("SimpleGitHunkPreview")
        frame = e.painted(lambda f: bool(f.get("popups")))
        popup = self.native_popup(frame)
        self.assertTrue(any("+new" in run["text"] and run["style"]["foreground"] == 0x45d279 for run in popup["runs"]))
        self.assert_event_marker(popup, "diff")
        requests = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertTrue(any(request["type"] == "hunks" and request["path"] == str(e.file) for request in requests))
        self.assert_source_unchanged()
        e.keys("j")
        e.painted(lambda f: f["supported"] and f.get("popups") == [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
