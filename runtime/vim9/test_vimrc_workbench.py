#!/usr/bin/env python3
"""Cross-repository integration with beamiter/vimrc and real simple* plugins.

Place vimrc, simplecc, simplegit and simplewhichkey beside jterm_core, or set
VIM9_TEST_VIMRC to the vimrc checkout and VIM9_TEST_SIMPLE_ROOT to their parent.
The test never installs plugins, starts a language server or contacts a remote
machine. SimpleCC's own deterministic daemon fixture supplies its replies;
the Vim configuration, plugin runtime, commands and authenticated bridge are
the real implementations. Run this optional suite from any working directory.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import tempfile
import unittest

from test_bridge import Editor, VIM


CHECKOUT_PARENT = Path(__file__).resolve().parents[3]
VIMRC_ROOT = Path(os.environ.get("VIM9_TEST_VIMRC", CHECKOUT_PARENT / "vimrc")).resolve()
PLUGIN_ROOT = Path(os.environ.get("VIM9_TEST_SIMPLE_ROOT", CHECKOUT_PARENT)).resolve()
PLUGINS = ("simplecc", "simplegit", "simplewhichkey")
AVAILABLE = VIM and (VIMRC_ROOT / ".vimrc").is_file() and all(
    (PLUGIN_ROOT / name / "plugin" / f"{name}.vim").is_file() for name in PLUGINS
)


def vim_string(value: str | Path) -> str:
    return "'" + str(value).replace("'", "''") + "'"


@unittest.skipUnless(AVAILABLE, "Needs Vim 9.1 and sibling vimrc/simplecc/simplegit/simplewhichkey checkouts")
class VimrcWorkbenchTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="jterm-simple-integration-")
        self.root = Path(self.directory.name)
        self.home = self.root / "home"
        self.home.mkdir()
        self.editors: list[Editor] = []

    def tearDown(self):
        for editor in self.editors:
            editor.close()
        self.directory.cleanup()

    def editor(self, *, authenticate=True, start_cc=False):
        # Disable all automatic installation/update/network activity. Only the
        # three checked-out plugin runtimes enter runtimepath. Existing user
        # configuration and caches are isolated by the child process's HOME.
        setup = [
            "let g:vimrc_skip_plugins = 1",
            "let g:vimrc_simpleplug_auto_bootstrap = 0",
            "let g:simpleplug_auto_install = 0",
            "let g:simplecc_auto_start = 0",
            "let g:simplecc_auto_complete = 0",
            "let g:simplecc_auto_restart = 0",
            "let g:simplegit_auto_enable = 0",
            "let g:simplegit_line_blame = 0",
            "let g:simplegit_watch = 0",
            "let g:simplewhichkey_delay = 5000",
            "let g:test_ready_count = 0",
            "let g:test_disconnect_count = 0",
        ]
        for name in PLUGINS:
            setup.append("let &runtimepath = " + vim_string(PLUGIN_ROOT / name) + " .. ',' .. &runtimepath")
        setup.extend([
            "execute 'source ' .. fnameescape(" + vim_string(VIMRC_ROOT / ".vimrc") + ")",
            "set nomore",
            "augroup TestWorkbenchLifecycle",
            "autocmd!",
            "autocmd User JTermVim9Ready let g:test_ready_count += 1",
            "autocmd User JTermVim9Disconnected let g:test_disconnect_count += 1",
            "augroup END",
        ])
        if start_cc:
            fixture = self.root / "simplecc-fixture"
            fixture.write_bytes((PLUGIN_ROOT / "simplecc/test/fake_daemon.sh").read_bytes())
            fixture.chmod(0o700)
            setup.extend([
                "let g:simplecc_daemon_path = " + vim_string(fixture),
                "autocmd VimEnter * setfiletype rust | SimpleCCStart",
            ])
        e = Editor(
            self.root,
            authenticate=authenticate,
            vimrc_extra="\n".join(setup) + "\n",
            extra_env={
                "HOME": str(self.home),
                "XDG_STATE_HOME": str(self.root / "state"),
                "XDG_CONFIG_HOME": str(self.root / "config"),
                "VIMRC_SKIP_PLUGINS": "1",
                "VIMRC_SKIP_UPDATE_CHECK": "1",
            },
        )
        self.editors.append(e)
        return e

    def capture(self, e: Editor, expression: str):
        output = self.root / "capture.json"
        e.ex("call writefile([json_encode(" + expression + ")], " + vim_string(output) + ")")
        return json.loads(output.read_text(encoding="utf-8"))

    def workbench(self, e: Editor, command: str, *, state=None, ok=True):
        source = state or e.snapshot()
        return e.request("workbench", command=command, ok=ok, **{
            name: source[name] for name in ("window", "buffer", "changedtick")
        })

    def test_real_configuration_handshake_and_native_report(self):
        e = self.editor(authenticate=False)
        self.assertFalse(e.state, "provider metadata must not precede authentication")
        e.authenticate()
        state = e.snapshot()
        self.assertEqual(state["workbench"]["provider"], "simple")
        self.assertTrue(state["workbench"]["native"])
        self.assertIn("simple_workbench", state["capabilities"])
        self.assertIn("simple_keys", state["capabilities"])
        self.assertNotIn("simple_files", state["capabilities"], "a missing finder must not be advertised")
        self.assertEqual(self.capture(e, "g:test_ready_count"), 1)

        self.workbench(e, "simple_workbench")
        e.state_where(lambda s: s.get("filetype") == "vimrcreport")
        report = self.capture(e, "getline(1, '$')")
        self.assertTrue(any("authenticated native Vim bridge" in line for line in report), report)
        frame = e.gui_frame()
        self.assertTrue(frame["supported"], frame["fallback_reason"])
        self.assertTrue(any("Simple Workbench" in run["text"] for run in frame["runs"]))
        e.keys("q")
        e.state_where(lambda s: s.get("buffer") == state["buffer"])
        self.assertEqual(self.capture(e, "getline(1)"), "original line")

    def test_simplecc_definition_and_diagnostics_use_real_plugin_runtime(self):
        e = self.editor(start_cc=True)
        e.state_where(lambda s: s.get("workbench", {}).get("diagnostics", {}).get("error") == 1)
        state = e.snapshot()
        self.assertIn("definition", state["capabilities"])
        self.assertIn("simple_rename", state["capabilities"])
        self.assertNotIn("rename", state["capabilities"], "prompt-based rename is not rename{name}")
        self.assertEqual(state["workbench"]["diagnostics"]["warning"], 3)
        self.assertEqual(state["workbench"]["diagnostics"]["hint"], 2)
        # Real SimpleCCDefinition sends the fixture a textDocument/definition
        # request. Its asynchronous result moves Vim through the plugin's own
        # response handler; no test definition/hover command replaces it.
        e.request("definition")
        e.state_where(lambda s: s.get("line") == 2)
        self.assertEqual(self.capture(e, "line('.')"), 2)

    def test_context_cache_never_crosses_buffer_boundary(self):
        e = self.editor()
        e.ex("let b:simplegit_status_dict = {'head':'integration/中文','ahead':2,'added':4,'changed':3,'removed':1}")
        e.ex("let b:simplecc_diag_counts = {'error':2,'warning':3,'info':0,'hint':1}")
        first = e.snapshot()
        self.assertEqual(first["workbench"]["git"]["head"], "integration/中文")
        self.assertEqual(first["workbench"]["diagnostics"]["error"], 2)
        e.request("open", path=str(e.other))
        second = e.snapshot()
        self.assertNotEqual(first["buffer"], second["buffer"])
        self.assertEqual(second["workbench"]["git"]["head"], "")
        self.assertEqual(second["workbench"]["diagnostics"]["error"], 0)
        for key in ("window", "buffer", "changedtick"):
            self.assertEqual(second["workbench"][key], second[key])
        reply = self.workbench(e, "simple_workbench", state=first, ok=False)
        self.assertIn("palette", reply["error"].lower())
        self.assertEqual(e.snapshot()["buffer"], second["buffer"])

    def test_reloading_preserves_maps_and_refreshes_disabled_actions(self):
        e = self.editor()
        maps = "[maparg('<Space>fs', 'n'), maparg('<C-h>', 'n'), maparg('<Space>vw', 'n')]"
        before = self.capture(e, maps)
        self.assertIn("update", before[0])
        self.assertIn("VimrcWorkbench", before[2])
        for _ in range(2):
            e.ex("execute 'source ' .. fnameescape(" + vim_string(VIMRC_ROOT / ".vimrc") + ")")
        after = self.capture(e, maps)
        self.assertEqual(before, after)
        self.assertEqual(self.capture(e, "g:test_ready_count"), 1)
        e.ex("let g:vimrc_workbench_disabled_actions = ['simple_keys']")
        self.assertNotIn("simple_keys", e.snapshot()["capabilities"])
        self.workbench(e, "simple_keys", ok=False)
        e.ex("let g:vimrc_workbench_enable = 0")
        state = e.snapshot()
        self.assertFalse(any(cap.startswith("simple_") for cap in state["capabilities"]))
        self.assertEqual(state.get("workbench", {}).get("provider", ""), "")

    def test_unknown_action_does_not_execute_ex_text(self):
        e = self.editor()
        state = e.snapshot()
        self.workbench(e, "simple_workbench|call setline(1, 'injected')", state=state, ok=False)
        self.assertEqual(self.capture(e, "getline(1)"), "original line")
        self.assertEqual(e.snapshot()["buffer"], state["buffer"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
