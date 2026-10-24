#!/usr/bin/env python3
"""Real Simple plugin actions and metadata through the authenticated Vim bridge.

Only daemon replies use each provider's checked-in deterministic fixture. The
Vim configuration, plugin commands, caches, confirmation and bridge are real.
No plugin is downloaded, installed in a user's home, or connected to a server.
"""

from __future__ import annotations

import json
from pathlib import Path
import time
import unittest

import test_vimrc_workbench as workbench


PLUGINS = workbench.PLUGINS + ("simpleplug", "simpletree", "simpleline")
AVAILABLE = workbench.AVAILABLE and all(
    (workbench.PLUGIN_ROOT / name / "plugin" / f"{name}.vim").is_file()
    for name in PLUGINS
)


@unittest.skipUnless(AVAILABLE, "Needs sibling Simple plugin checkouts and full Vim 9.1+")
class SimpleEcosystemTests(unittest.TestCase):
    setUp = workbench.VimrcWorkbenchTests.setUp
    tearDown = workbench.VimrcWorkbenchTests.tearDown
    capture = workbench.VimrcWorkbenchTests.capture
    assert_pty_text = workbench.VimrcWorkbenchTests.assert_pty_text
    workbench = workbench.VimrcWorkbenchTests.workbench

    def editor(self, *, extra_setup="", extra_env=None):
        return workbench.VimrcWorkbenchTests.editor(
            self, plugins=PLUGINS, extra_env=extra_env,
            extra_setup="\n".join([
                "let g:simpleline_auto_enable = 0",
                "let g:simpleline_git_enabled = 0",
                "let g:simpletree_persist_width = 0",
                "let g:simpletree_auto_refresh = 0",
                "let g:simpletree_git_status = 0",
                "let g:simpleplug_activate_on_install = 0",
                extra_setup,
            ]),
        )

    def fixture(self, plugin: str, relative: str) -> Path:
        target = self.root / f"{plugin}-fixture"
        target.write_bytes((workbench.PLUGIN_ROOT / plugin / relative).read_bytes())
        target.chmod(0o700)
        return target

    def requests(self, path: Path, kind: str | None = None):
        rows = [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
        return [row for row in rows if kind is None or row.get("type") == kind]

    def until(self, editor, predicate, timeout=4):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate():
                return
            editor.pump(0.025)
        self.fail("Timed out waiting for the real plugin operation")

    def test_idle_snapshots_do_not_start_plugin_services_or_invent_results(self):
        marker = self.root / "unexpected-service-start"
        trap = self.root / "service-trap"
        trap.write_text("#!/bin/sh\nprintf started >> '" + str(marker) + "'\nexit 2\n")
        trap.chmod(0o700)
        e = self.editor(extra_setup="\n".join(
            "let g:" + name + "_daemon_path = " + workbench.vim_string(trap)
            for name in ("simplecc", "simplegit", "simpleplug", "simpletree", "simpleline")
        ))
        before = self.capture(e, "len(job_info())")
        for _ in range(4):
            state = e.snapshot()
            services = state["workbench"]
            self.assertTrue(services["lsp"]["available"])
            self.assertTrue(services["plugins"]["available"])
            self.assertFalse(services["plugins"]["running"])
            self.assertEqual(services["plugins"]["operation"], "")
            self.assertEqual(services["plugins"]["total"], 0)
            self.assertFalse(services["tree"]["visible"])
            self.assertFalse(services["statusline"]["enabled"])
            self.assertNotIn("simple_plugin_stop", state["capabilities"])
            self.assertIn("simple_plugin_health", state["capabilities"])
            self.assertIn("simple_statusline_toggle", state["capabilities"])
            self.assertIn("simple_lsp_status", state["capabilities"])
        self.assertEqual(self.capture(e, "len(job_info())"), before)
        self.assertFalse(marker.exists(), "Reading metadata must not start even one provider")

    def test_statusline_toggle_preserves_editor_and_publishes_actual_enabled_state(self):
        e = self.editor()
        e.ex("set statusline=original-status showtabline=0 laststatus=1 cmdheight=3")
        source = e.snapshot()
        original = self.capture(e, "[&g:statusline, &g:tabline, &laststatus, &showtabline]")
        self.workbench(e, "simple_statusline_toggle")
        enabled = e.snapshot()
        self.assertTrue(enabled["workbench"]["statusline"]["enabled"])
        self.assertEqual(enabled["buffer"], source["buffer"])
        self.assertEqual(enabled["window"], source["window"])
        self.assertTrue(self.capture(e, "simpleline#IsEnabled()"))
        self.workbench(e, "simple_statusline_toggle")
        disabled = e.snapshot()
        self.assertFalse(disabled["workbench"]["statusline"]["enabled"])
        self.assertEqual(self.capture(e, "[&g:statusline, &g:tabline, &laststatus, &showtabline]"), original)
        self.assertEqual(self.capture(e, "getline(1, '$')"), ["original line", "second line"])

    def git_editor(self):
        (self.root / ".git").mkdir()
        fixture = self.fixture("simplegit", "tests/simplegit_fake_daemon.py")
        log = self.root / "git.requests"
        hunks = [
            {"old_start": 0, "old_count": 0, "new_start": 1, "new_count": 1,
             "lines": ["@@ -0,0 +1 @@", "+original line"]},
            {"old_start": 2, "old_count": 1, "new_start": 2, "new_count": 1,
             "lines": ["@@ -2 +2 @@", "-old", "+second line"]},
        ]
        e = self.editor(extra_setup="let g:simplegit_daemon_path = " + workbench.vim_string(fixture),
                        extra_env={"SIMPLEGIT_FAKE_LOG": str(log),
                                   "SIMPLEGIT_FAKE_HUNKS": json.dumps(hunks)})
        e.ex("SimpleGitEnable")
        self.until(e, lambda: e.state.get("workbench", {}).get("git", {}).get("changed") == 1)
        return e, log

    def test_git_navigation_stage_and_stale_source_use_real_provider(self):
        e, log = self.git_editor()
        initial = e.snapshot()
        self.assertIn("simple_git_stage_hunk", initial["capabilities"])
        self.workbench(e, "simple_git_next_hunk")
        self.assertEqual(e.snapshot()["line"], 2)
        self.workbench(e, "simple_git_stage_hunk", state=initial, ok=False)
        self.assertEqual(self.requests(log, "stage"), [])
        self.workbench(e, "simple_git_stage_hunk")
        self.until(e, lambda: len(self.requests(log, "stage")) == 1)
        self.assertEqual(self.requests(log, "stage")[0]["lnum"], 2)
        self.assertEqual(self.requests(log, "stage")[0]["path"], str(e.file))
        self.workbench(e, "simple_git_previous_hunk")
        self.assertEqual(e.snapshot()["line"], 1)
        e.keys(b"A!\x1b")
        e.state_where(lambda s: s.get("modified"))
        dirty = e.snapshot()
        self.assertNotIn("simple_git_stage_hunk", dirty["capabilities"])
        self.assertNotIn("simple_git_undo_hunk", dirty["capabilities"])
        self.workbench(e, "simple_git_stage_hunk", ok=False)
        self.assertEqual(len(self.requests(log, "stage")), 1)
        # SimpleGit computes live buffer hunks. Editing must keep navigation
        # available even though disk-mutating stage/discard remain guarded.
        self.until(e, lambda: "simple_git_next_hunk" in e.state.get("capabilities", []))
        self.workbench(e, "simple_git_next_hunk")
        self.assertEqual(e.snapshot()["line"], 2)
        self.assertTrue(e.snapshot()["modified"])

    def test_git_discard_defaults_to_cancel_and_leaves_file_untouched(self):
        e, log = self.git_editor()
        source = e.snapshot()
        original_file = e.file.read_bytes()
        request_id = e.next_id
        e.next_id += 1
        e.screen.clear()
        e.send({"id": request_id, "action": "workbench", "command": "simple_git_undo_hunk",
                **{name: source[name] for name in ("window", "buffer", "changedtick", "line", "column")}})
        self.assert_pty_text(e, b"Discard the saved Git hunk")
        e.keys(b"\r")
        response = e.wait(lambda m: m.get("event") == "response" and m.get("id") == request_id)
        self.assertTrue(response["ok"], response)
        self.assertEqual(self.requests(log, "undo"), [])
        self.assertEqual(e.file.read_bytes(), original_file)
        self.assertEqual(e.snapshot()["buffer"], source["buffer"])
        self.assertTrue(e.gui_frame()["supported"])

    def test_git_discard_rechecks_source_after_confirmation(self):
        e, log = self.git_editor()
        # Timers still run while Vim's confirmation owns input. Simulate an
        # asynchronous editor update after the user first saw the hunk action.
        e.ex("call timer_start(400, {_ -> setline(1, 'new source')})")
        source = e.snapshot()
        original_file = e.file.read_bytes()
        request_id = e.next_id
        e.next_id += 1
        e.screen.clear()
        e.send({"id": request_id, "action": "workbench", "command": "simple_git_undo_hunk",
                **{name: source[name] for name in ("window", "buffer", "changedtick", "line", "column")}})
        self.assert_pty_text(e, b"Discard the saved Git hunk")
        deadline = time.monotonic() + 0.7
        while time.monotonic() < deadline:
            e.pump(0.025)
        e.keys("d")
        response = e.wait(lambda m: m.get("event") == "response" and m.get("id") == request_id)
        self.assertFalse(response["ok"], response)
        self.assertIn("source changed", response["error"])
        self.assertEqual(self.requests(log, "undo"), [])
        self.assertEqual(e.file.read_bytes(), original_file)

    def test_plugin_batch_reports_live_progress_and_does_not_run_twice(self):
        fixture = self.fixture("simpleplug", "tests/fake_plug_daemon.py")
        log = self.root / "plug.requests"
        e = self.editor(extra_setup="let g:simpleplug_daemon_path = " + workbench.vim_string(fixture),
                        extra_env={"FAKE_PLUG_DUMP": str(log), "FAKE_PLUG_DELAY_MS": "500",
                                   "FAKE_PLUG_FAIL": "beta"})
        e.ex("call simpleplug#Begin(" + workbench.vim_string(self.root / "plugged") + ")")
        e.ex("call simpleplug#Plug('local/alpha')")
        e.ex("call simpleplug#Plug('local/beta')")
        e.ex("call simpleplug#End()")
        before = e.snapshot()
        self.assertFalse(before["workbench"]["plugins"]["running"])
        self.assertEqual(self.requests(log), [])
        self.workbench(e, "simple_plugin_install")
        running = e.snapshot()
        self.assertTrue(running["workbench"]["plugins"]["known"])
        self.assertTrue(running["workbench"]["plugins"]["running"])
        self.assertEqual(running["workbench"]["plugins"]["total"], 2)
        self.assertIn("simple_plugin_stop", running["capabilities"])
        self.assertNotIn("simple_plugin_update", running["capabilities"])
        self.workbench(e, "simple_plugin_update", ok=False)
        finished = e.state_where(lambda s: s.get("workbench", {}).get("plugins", {}).get("errors") == 1)
        plugins = finished["workbench"]["plugins"]
        self.assertFalse(plugins["running"])
        self.assertEqual(plugins["operation"], "install")
        self.assertEqual(plugins["installed"], 1)
        self.assertEqual(plugins["completed"], 2)
        self.assertNotIn("simple_plugin_stop", finished["capabilities"])
        for _ in range(3):
            self.assertEqual(e.snapshot()["workbench"]["plugins"], plugins)
        self.assertEqual(len(self.requests(log, "install")), 1)
        self.assertEqual(self.requests(log, "update"), [])
        e.keys("q")
        e.state_where(lambda s: s.get("buffer") == before["buffer"])

    def test_project_tree_state_tracks_actual_open_refresh_and_close(self):
        fixture = self.fixture("simpletree", "tests/fake_tree_daemon.py")
        e = self.editor(extra_setup="let g:simpletree_daemon_path = " + workbench.vim_string(fixture))
        source = e.snapshot()
        self.assertNotIn("simple_tree_refresh", source["capabilities"])
        self.workbench(e, "simple_tree_toggle")
        opened = e.snapshot()
        self.assertTrue(opened["workbench"]["tree"]["visible"])
        self.assertEqual(opened["workbench"]["tree"]["root"].rstrip("/"), str(self.root))
        self.assertIn("simple_tree_refresh", opened["capabilities"])
        # The provider's fixture deliberately streams its first listing over
        # 1.1 seconds. Wait for that bounded stream before choosing a command
        # from this tree buffer, whose changedtick legitimately moves on load.
        deadline = time.monotonic() + 1.3
        while time.monotonic() < deadline:
            e.pump(0.025)
        self.workbench(e, "simple_tree_refresh")
        self.assertTrue(e.snapshot()["workbench"]["tree"]["visible"])
        self.workbench(e, "simple_tree_toggle")
        closed = e.snapshot()
        self.assertFalse(closed["workbench"]["tree"]["visible"])
        self.assertEqual(closed["buffer"], source["buffer"])
        self.assertNotIn("simple_tree_refresh", closed["capabilities"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
