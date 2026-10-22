#!/usr/bin/env python3
"""Real SimpleTerminal/SimpleTree workbench actions over Vim's PTY.

Set VIM9_TEST_VIMRC and VIM9_TEST_SIMPLE_ROOT like test_vimrc_workbench.
No installers, network, or shell-provider stubs are used. SimpleTree uses an
existing daemon binary, falling back to its checked-in protocol fixture.
"""
from __future__ import annotations

import os
from pathlib import Path
import time
import unittest

from test_bridge import Editor, VIM
import test_vimrc_workbench as integration

VIMRC_ROOT = integration.VIMRC_ROOT
PLUGIN_ROOT = integration.PLUGIN_ROOT
PLUGINS = ("simpleterminal", "simpletree")
AVAILABLE = VIM and (VIMRC_ROOT / ".vimrc").is_file() and all(
    (PLUGIN_ROOT / name / "plugin" / f"{name}.vim").is_file() for name in PLUGINS
)


@unittest.skipUnless(AVAILABLE, "Needs Vim, vimrc, SimpleTerminal and SimpleTree checkouts")
class SimpleWorkspaceTests(unittest.TestCase):
    setUp = integration.VimrcWorkbenchTests.setUp
    tearDown = integration.VimrcWorkbenchTests.tearDown
    capture = integration.VimrcWorkbenchTests.capture
    workbench = integration.VimrcWorkbenchTests.workbench

    def editor(self):
        quote = integration.vim_string
        tree = PLUGIN_ROOT / "simpletree"
        daemon = next((tree / name for name in (
            "target/release/simpletree-daemon", "target/debug/simpletree-daemon",
            "lib/simpletree-daemon") if os.access(tree / name, os.X_OK)), None)
        if daemon is None:
            fixture = tree / "tests/fake_tree_daemon.py"
            if not fixture.is_file():
                self.skipTest("SimpleTree has neither daemon nor checked-in fixture")
            daemon = self.root / "tree-fixture"
            daemon.write_bytes(fixture.read_bytes())
            daemon.chmod(0o700)
        setup = [
            "let g:vimrc_skip_plugins = 1",
            "let g:vimrc_simpleplug_auto_bootstrap = 0",
            "let g:simpleplug_auto_install = 0",
            "let g:simpleterminal_shell = '/bin/sh'",
            "let g:simpleterminal_prefer_remote = 0",
            "let g:simpletree_persist_width = 0",
            "let g:simpletree_daemon_path = " + quote(daemon),
            "let g:simpletree_state_file = " + quote(self.root / "tree-state.json"),
        ]
        for name in PLUGINS:
            setup.append("let &runtimepath = " + quote(PLUGIN_ROOT / name) + " .. ',' .. &runtimepath")
        setup.append("execute 'source ' .. fnameescape(" + quote(VIMRC_ROOT / ".vimrc") + ")")
        editor = Editor(self.root, vimrc_extra="\n".join(setup) + "\n", extra_env={
            "HOME": str(self.home), "XDG_STATE_HOME": str(self.root / "state"),
            "XDG_CONFIG_HOME": str(self.root / "config"),
            "VIMRC_SKIP_PLUGINS": "1", "VIMRC_SKIP_UPDATE_CHECK": "1",
        })
        self.editors.append(editor)
        return editor

    def terminal_input(self, e, marker):
        state = e.state_where(lambda s: s.get("mode") in ("t", "nt"))
        # Vim preserves terminal-normal mode when reusing a hidden terminal.
        if state["mode"] == "nt":
            e.keys("i")
            state = e.state_where(lambda s: s.get("mode") == "t")
        self.assertFalse(e.gui_frame()["supported"])
        e.keys("printf 'native-%s\\n' '" + marker + "'\r")
        expected = ("native-" + marker).encode()
        deadline = time.monotonic() + 5
        while expected not in e.screen and time.monotonic() < deadline:
            e.pump(0.1)
        self.assertIn(expected, e.screen)
        return state["buffer"]

    def hide_terminal(self, e, source):
        e.keys(b"\x1b[19~")  # xterm F8: the real plugin's terminal mapping.
        e.state_where(lambda s: s.get("mode") == "n" and s.get("buffer") == source["buffer"])
        self.assertTrue(e.gui_frame()["supported"])
        state = self.capture(e, "simpleterminal#State()")
        self.assertEqual(state["popup"], 0)
        return state

    def test_real_terminal_pty_f8_and_existing_session_reuse(self):
        e = self.editor()
        source = e.snapshot()
        self.assertIn("simple_terminal_toggle", source["capabilities"])
        self.workbench(e, "simple_terminal_toggle", state=source)
        first = self.terminal_input(e, "first-session")
        before = self.hide_terminal(e, source)
        self.assertEqual([item["bufnr"] for item in before["sessions"]], [first])

        self.workbench(e, "simple_terminal_toggle")
        self.assertEqual(self.terminal_input(e, "first-reused"), first)
        reused = self.hide_terminal(e, source)
        self.assertEqual([item["bufnr"] for item in reused["sessions"]], [first])

        self.workbench(e, "simple_terminal_new")
        second = self.terminal_input(e, "second-session")
        self.assertNotEqual(first, second)
        self.hide_terminal(e, source)
        self.workbench(e, "simple_terminal_next")
        self.assertEqual(self.terminal_input(e, "first-after-next"), first)
        self.hide_terminal(e, source)
        self.workbench(e, "simple_terminal_previous")
        self.assertEqual(self.terminal_input(e, "second-after-previous"), second)
        after = self.hide_terminal(e, source)
        self.assertEqual([item["bufnr"] for item in after["sessions"]], [first, second])
        self.assertTrue(all(item["running"] for item in after["sessions"]))
        first_text = "\n".join(self.capture(e, f"getbufline({first}, 1, '$')"))
        second_text = "\n".join(self.capture(e, f"getbufline({second}, 1, '$')"))
        self.assertIn("native-first-after-next", first_text)
        self.assertNotIn("native-second-after-previous", first_text)
        self.assertIn("native-second-after-previous", second_text)
        self.assertNotIn("native-first-after-next", second_text)
        self.assertEqual(self.capture(e, "getline(1)"), "original line")

    def test_real_tree_toggle_reveal_and_native_return(self):
        e = self.editor()
        source = e.snapshot()
        self.assertIn("simple_tree_toggle", source["capabilities"])
        self.assertIn("simple_tree_reveal", source["capabilities"])
        self.workbench(e, "simple_tree_toggle", state=source)
        e.state_where(lambda s: s.get("filetype") == "simpletree")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            lines = self.capture(e, "getline(1, '$')")
            if any("main.txt" in line for line in lines):
                break
            e.pump(0.1)
        self.assertTrue(any("main.txt" in line for line in lines), lines)
        self.assertTrue(e.gui_frame()["supported"])
        # Reveal is the plugin's existing operation on an open tree. Invoke
        # it from the real source window, keeping the sidebar alive.
        e.ex("wincmd p")
        self.assertEqual(e.snapshot()["buffer"], source["buffer"])
        self.workbench(e, "simple_tree_reveal")
        # Reveal updates the tree cursor while preserving source-window focus.
        tree = self.capture(e, "filter(getwininfo(), {_, w -> getbufvar(w.bufnr, '&filetype') ==# 'simpletree'})[0]")
        selected = self.capture(e, f"getbufline({tree['bufnr']}, getcurpos({tree['winid']})[1])")
        self.assertTrue(any("main.txt" in line for line in selected), selected)
        self.assertEqual(e.snapshot()["buffer"], source["buffer"])
        # A real filesystem watcher update shifts the selected node's row.
        # Do not send PTY keys or capture via Ex after the write: a native
        # action must remain usable when only background activity occurred.
        watched = self.root / "000-native-watch.txt"
        watched.write_text("watcher update\n", encoding="utf-8")
        e.wait(lambda message: message.get("event") == "render"
               and message.get("supported")
               and any(watched.name in run.get("text", "")
                       for run in message.get("runs", [])))
        self.workbench(e, "simple_tree_toggle")
        e.state_where(lambda s: s.get("buffer") == source["buffer"])
        self.assertTrue(e.gui_frame()["supported"])
        self.assertEqual(self.capture(e, "getline(1)"), "original line")


if __name__ == "__main__":
    unittest.main(verbosity=2)
