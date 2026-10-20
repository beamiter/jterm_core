#!/usr/bin/env python3
"""Exercise the shipped bridge against a real Vim 9 process in a real PTY.

No GUI, editor emulation, mocked Vim evaluator, third-party Python packages or
network services are needed.  Set VIM9_TEST_VIM to an explicit Vim executable.
The test uses one authenticated loopback connection per isolated editor.
"""

from __future__ import annotations

import collections
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
import termios
import time
import unittest


RUNTIME = Path(__file__).with_name("bridge.vim").resolve()
VIM = os.environ.get("VIM9_TEST_VIM") or shutil.which("vim")


class Editor:
    def __init__(self, root: Path, *, authenticate: bool = True, plugin: bool = False,
                 failing_rg: bool = False, rg_script: str | None = None,
                 extra_env: dict[str, str] | None = None):
        self.root = root
        self.file = root / "main.txt"
        self.file.write_text("original line\nsecond line\n", encoding="utf-8")
        self.other = root / "other.txt"
        self.other.write_text("other file\n", encoding="utf-8")
        vimrc = root / "user.vimrc"
        vimrc_text = (
            "set nocompatible nomore noswapfile nowrap\n"
            "set shortmess+=I\n"
            "set errorformat=%f:%l:%c:%m\n"
            "nnoremap Q :call setline(1, 'from-user-vimrc')<CR>\n"
        )
        if plugin:
            vimrc_text += (
                "command! LspGotoDefinition normal! G\n"
                "command! LspHover call writefile(['hover'], 'hover.marker')\n"
                "command! LspShowReferences call setqflist([{'filename': 'other.txt', 'lnum': 1, 'col': 2, 'text': 'reference', 'type': 'I'}])\n"
                "command! -nargs=? LspRename call writefile([<q-args>], 'rename.marker')\n"
                "command! LspFormat call setline(1, 'formatted by provider')\n"
            )
        vimrc.write_text(vimrc_text, encoding="utf-8")
        self.token = os.urandom(24).hex()
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(1)
        self.listener.settimeout(8)
        port = self.listener.getsockname()[1]
        env = dict(os.environ)
        env.update(
            TERM="xterm-256color",
            JTERM_VIM9_ADDR=f"127.0.0.1:{port}",
            JTERM_VIM9_TOKEN=self.token,
            JTERM_VIM9_ROOT=str(root),
        )
        if extra_env:
            env.update(extra_env)
        if failing_rg or rg_script is not None:
            test_tools = root / "test-tools"
            test_tools.mkdir()
            fake_rg = test_tools / "rg"
            fake_rg.write_text(rg_script if rg_script is not None else
                               "#!/bin/sh\nprintf 'test search process failure\\n' >&2\nexit 2\n")
            fake_rg.chmod(0o700)
            env["PATH"] = str(test_tools) + os.pathsep + env.get("PATH", "")
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        os.set_blocking(self.master, False)
        self.process = subprocess.Popen(
            [str(VIM), "-N", "-u", str(vimrc), "-i", "NONE", "-n", "-S", str(RUNTIME), "--", str(self.file)],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            cwd=root,
            env=env,
            start_new_session=True,
        )
        os.close(slave)
        self.messages: collections.deque[dict] = collections.deque()
        self.wire = b""
        self.screen = bytearray()
        self.state: dict = {}
        self.next_id = 1
        self.connection: socket.socket | None = None
        try:
            self.connection, _ = self.listener.accept()
            self.connection.setblocking(False)
            hello = self.wait(lambda m: m.get("event") == "hello")
            if hello.get("token") != self.token or hello.get("protocol") != 1 or hello.get("version", 0) < 900:
                raise AssertionError(f"Incorrect bridge hello: {hello}")
            if authenticate:
                self.authenticate()
        except BaseException:
            self.drain_screen()
            self.close()
            raise AssertionError(f"Vim bridge did not start. PTY output:\n{self.screen.decode(errors='replace')}")

    def drain_screen(self):
        while True:
            try:
                part = os.read(self.master, 65536)
                if not part:
                    break
                self.screen.extend(part)
                self.screen = self.screen[-65536:]
            except OSError as exc:
                if exc.errno in (errno.EIO, errno.EAGAIN, errno.EBADF):
                    break
                raise

    def pump(self, timeout: float):
        readers = [self.master]
        if self.connection is not None:
            readers.append(self.connection)
        ready, _, _ = select.select(readers, [], [], timeout)
        if self.master in ready:
            self.drain_screen()
        if self.connection is not None and self.connection in ready:
            data = self.connection.recv(65536)
            if not data:
                self.connection.close()
                self.connection = None
                return
            self.wire += data
            while b"\n" in self.wire:
                raw, self.wire = self.wire.split(b"\n", 1)
                if len(raw) >= 1024 * 1024:
                    raise AssertionError("Bridge exceeded the host frame limit")
                message = json.loads(raw)
                if message.get("event") == "state":
                    self.state = message
                self.messages.append(message)

    def wait(self, predicate, timeout: float = 6.0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for message in tuple(self.messages):
                if predicate(message):
                    self.messages.remove(message)
                    return message
            self.pump(min(0.1, max(0.0, deadline - time.monotonic())))
        raise AssertionError(
            f"Timed out waiting for Vim. Last state: {self.state}\n"
            f"Pending messages: {list(self.messages)[-10:]}\n"
            f"PTY: {self.screen.decode(errors='replace')[-8000:]}"
        )

    def send(self, payload: dict):
        if self.connection is None:
            raise AssertionError("Vim bridge disconnected")
        self.connection.sendall(json.dumps(payload, ensure_ascii=False).encode() + b"\n")

    def authenticate(self):
        self.send({"action": "auth", "token": self.token, "protocol": 1})
        return self.wait(lambda m: m.get("event") == "state")

    def request(self, action: str, *, ok: bool = True, **params):
        request_id = self.next_id
        self.next_id += 1
        self.send({"id": request_id, "action": action, **params})
        reply = self.wait(lambda m: m.get("event") == "response" and m.get("id") == request_id)
        if reply.get("ok") is not ok:
            raise AssertionError(f"Unexpected response to {action}: {reply}")
        return reply

    def snapshot(self):
        # Discard stale snapshots before requesting a forced current one.
        self.messages = collections.deque(m for m in self.messages if m.get("event") != "state")
        self.request("snapshot")
        return self.wait(lambda m: m.get("event") == "state")

    def keys(self, data: str | bytes):
        self.messages = collections.deque(m for m in self.messages if m.get("event") != "state")
        os.write(self.master, data.encode() if isinstance(data, str) else data)

    def state_where(self, predicate, timeout: float = 6.0):
        return self.wait(lambda m: m.get("event") == "state" and predicate(m), timeout)

    def ex(self, command: str):
        # Observe the command-line transition before Enter: a complete option
        # command can finish inside one coalesced tick without changing state.
        self.keys(":" + command)
        self.state_where(lambda s: s["mode"].startswith("c"))
        self.keys("\r")
        self.state_where(lambda s: s["mode"] == "n")

    def close(self):
        if self.process.poll() is None:
            try:
                self.keys(b"\x1b\x1b:qa!\r")
                deadline = time.monotonic() + 2
                while self.process.poll() is None and time.monotonic() < deadline:
                    self.drain_screen()
                    time.sleep(0.02)
            except OSError:
                pass
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGKILL)
        self.process.wait(timeout=3)
        if self.connection is not None:
            self.connection.close()
            self.connection = None
        self.listener.close()
        try:
            os.close(self.master)
        except OSError:
            pass


@unittest.skipUnless(VIM, "Set VIM9_TEST_VIM or install Vim 9 to run the real editor integration tests")
class BridgeTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="jterm-vim9-")
        self.root = Path(self.directory.name)
        self.editors: list[Editor] = []

    def editor(self, **kwargs):
        editor = Editor(self.root, **kwargs)
        self.editors.append(editor)
        return editor

    def tearDown(self):
        for editor in self.editors:
            editor.close()
        self.directory.cleanup()

    def test_authentication_and_explicit_dispatch(self):
        e = self.editor(authenticate=False)
        rejected = e.request("save", ok=False)
        self.assertIn("authentication", rejected["error"])
        e.authenticate()
        e.request("ex", ok=False, command="call writefile(['bad'], 'PWNED')")
        self.assertFalse((self.root / "PWNED").exists())
        e.connection.sendall(b"[]\n")
        malformed = e.wait(lambda m: m.get("event") == "response" and m.get("id") == 0)
        self.assertFalse(malformed["ok"])
        snapshot = e.snapshot()
        self.assertEqual(snapshot["root"], str(self.root))
        self.assertIsInstance(snapshot["modified"], bool)
        self.assertTrue(snapshot["buffers"][0]["current"])

    def test_wrong_token_closes_channel(self):
        e = self.editor(authenticate=False)
        e.send({"action": "auth", "token": "wrong", "protocol": 1})
        response = e.wait(lambda m: m.get("event") == "response")
        self.assertFalse(response["ok"])
        deadline = time.monotonic() + 3
        while e.connection is not None and time.monotonic() < deadline:
            e.pump(0.05)
        self.assertIsNone(e.connection)
        self.assertIsNone(e.process.poll(), "A bridge failure must not kill the editor")

    def test_native_edit_undo_save_and_user_vimrc(self):
        e = self.editor()
        e.keys(b"ihello \x1b")
        e.state_where(lambda s: s["modified"] and s["mode"] == "n")
        e.request("save")
        self.assertEqual(e.file.read_text(), "hello original line\nsecond line\n")
        e.keys("u")
        e.state_where(lambda s: s["modified"] and s["mode"] == "n")
        e.request("save")
        self.assertEqual(e.file.read_text(), "original line\nsecond line\n")
        e.keys("Q")
        e.state_where(lambda s: s["modified"] and s["mode"] == "n")
        e.request("save")
        self.assertTrue(e.file.read_text().startswith("from-user-vimrc\n"))

    def test_escaped_filename_and_validated_positions(self):
        e = self.editor()
        strange = self.root / 'odd #% | call writefile(["bad"],"PWNED") |.txt'
        strange.write_text("first\nsecond needle\n", encoding="utf-8")
        e.request("open", path=str(strange), line=2, column=8)
        state = e.snapshot()
        self.assertEqual(state["file"], str(strange))
        self.assertEqual((state["line"], state["column"]), (2, 8))
        self.assertFalse((self.root / "PWNED").exists())
        e.request("open", ok=False, path=str(e.file), line=-1)
        self.assertEqual(e.snapshot()["file"], str(strange))
        e.request("buffer", buffer=1)
        self.assertEqual(e.snapshot()["file"], str(e.file))
        e.request("buffer", ok=False, buffer=100000)
        # No explicit coordinates must preserve Vim's own remembered cursor.
        e.request("open", path=str(strange))
        state = e.snapshot()
        self.assertEqual((state["line"], state["column"]), (2, 8))
        e.keys("0l")
        state = e.state_where(lambda s: s["line"] == 2 and s["column"] == 2)
        e.request("open", path=str(strange))
        self.assertEqual(e.snapshot()["column"], 2)
        current_id = next(b["id"] for b in state["buffers"] if b["current"])
        e.request("buffer", buffer=current_id)
        self.assertEqual(e.snapshot()["column"], 2)

    def test_modified_buffer_and_quit_are_not_forced(self):
        e = self.editor()
        e.keys(b"ichanged \x1b")
        e.state_where(lambda s: s["modified"] and s["mode"] == "n")
        e.request("open", ok=False, path=str(e.other))
        self.assertEqual(e.snapshot()["file"], str(e.file))
        e.request("quit", ok=False)
        self.assertIsNone(e.process.poll())
        e.request("save")
        e.request("quit")
        e.process.wait(timeout=3)
        self.assertEqual(e.process.returncode, 0)

    def test_pending_operator_and_command_line_are_preserved(self):
        e = self.editor()
        e.keys("d")
        e.state_where(lambda s: s["mode"].startswith("no"))
        e.request("open", ok=False, path=str(e.other))
        e.keys("w")
        e.state_where(lambda s: s["modified"] and s["mode"] == "n")
        e.request("save")
        self.assertTrue(e.file.read_text().startswith("line\n"))
        e.keys(":let g:test_pending = ")
        e.state_where(lambda s: s["mode"].startswith("c"))
        e.request("save", ok=False)
        e.keys(b"42\r")
        e.state_where(lambda s: s["mode"] == "n")
        e.keys(":call writefile([string(g:test_pending)], 'pending.marker')\r")
        deadline = time.monotonic() + 3
        marker = self.root / "pending.marker"
        while (not marker.exists() or marker.read_text() != "42\n") and time.monotonic() < deadline:
            e.pump(0.05)
        self.assertEqual((self.root / "pending.marker").read_text(), "42\n")

    def test_save_all_respects_users_hidden_option(self):
        e = self.editor()
        e.keys(":set hidden\r")
        e.keys(b"ifirst \x1b")
        e.state_where(lambda s: s["modified"] and s["mode"] == "n")
        e.request("open", path=str(e.other))
        e.keys(b"isecond \x1b")
        e.state_where(lambda s: s["file"] == str(e.other) and s["modified"] and s["mode"] == "n")
        e.request("save_all")
        self.assertTrue(e.file.read_text().startswith("first original"))
        self.assertTrue(e.other.read_text().startswith("second other"))
        self.assertFalse(any(b["modified"] for b in e.snapshot()["buffers"]))

    def test_native_goto_and_splits_keep_the_actual_modified_buffer(self):
        e = self.editor()
        e.keys(b"iunsaved \x1b")
        e.state_where(lambda s: s["modified"] and s["mode"] == "n")
        e.request("goto", line=2, column=4)
        state = e.snapshot()
        self.assertEqual((state["line"], state["column"], state["line_count"]), (2, 4, 2))
        self.assertTrue(state["modified"])
        self.assertEqual(state["encoding"], "utf-8")
        for params in [{}, {"line": 0}, {"line": "2"}, {"line": 1, "column": 0}]:
            e.request("goto", ok=False, **params)
        self.assertEqual(e.snapshot()["line"], 2)
        e.request("split", vertical=True)
        self.assertEqual(e.snapshot()["window_count"], 2)
        e.request("split", vertical=False)
        state = e.snapshot()
        self.assertEqual(state["window_count"], 3)
        self.assertTrue(state["modified"])
        self.assertEqual(len(state["buffers"]), 1)
        e.request("split", ok=False, vertical="yes")
        self.assertEqual(e.snapshot()["window_count"], 3)
        e.request("save")
        self.assertTrue(e.file.read_text().startswith("unsaved original"))
        e.keys(":enew\r")
        e.state_where(lambda s: s["file"] == "")
        e.keys(b"iunnamed\rsecond\x1b")
        e.state_where(lambda s: s["file"] == "" and s["modified"] and s["mode"] == "n")
        e.request("goto", line=1, column=3)
        state = e.snapshot()
        self.assertEqual((state["file"], state["line"], state["column"]), ("", 1, 3))
        self.assertTrue(state["modified"])

    def test_buffer_cycle_restores_views_and_close_refuses_hidden_dirty_data(self):
        e = self.editor()
        e.request("goto", line=2, column=5)
        original_id = next(b["id"] for b in e.snapshot()["buffers"] if b["current"])
        e.request("open", path=str(e.other))
        e.request("previous_buffer")
        state = e.snapshot()
        self.assertEqual((state["file"], state["line"], state["column"]), (str(e.file), 2, 5))
        e.request("next_buffer")
        self.assertEqual(e.snapshot()["file"], str(e.other))
        e.request("previous_buffer")
        e.keys(b"iKEEP \x1b")
        e.state_where(lambda s: s["modified"] and s["mode"] == "n")
        e.request("next_buffer", ok=False)
        self.assertEqual(e.snapshot()["file"], str(e.file))
        e.ex("set hidden")
        e.request("next_buffer")
        refused = e.request("close_buffer", ok=False, buffer=original_id)
        self.assertIn("Unsaved changes", refused["error"])
        self.assertTrue(any(b["id"] == original_id and b["modified"] for b in e.snapshot()["buffers"]))
        e.request("save_all")
        e.request("close_buffer", buffer=original_id)
        self.assertNotIn(original_id, [b["id"] for b in e.snapshot()["buffers"]])
        self.assertIn("KEEP ", e.file.read_text())
        e.request("close_buffer", ok=False, buffer=original_id)
        e.request("close_buffer", ok=False, buffer=0)
        current_id = next(b["id"] for b in e.snapshot()["buffers"] if b["current"])
        e.request("close_buffer", buffer=current_id)
        state = e.snapshot()
        self.assertEqual(state["file"], "")
        self.assertEqual(len(state["buffers"]), 1)
        self.assertIsNone(e.process.poll(), "Closing the last buffer must keep Vim alive")

    def test_native_navigation_shares_vims_jumplist_and_exact_positions(self):
        e = self.editor()
        e.ex("clearjumps")
        self.assertEqual(e.snapshot()["navigation"], {"can_go_back": False, "can_go_forward": False})
        e.request("navigate_back", ok=False)
        e.request("navigate_forward", ok=False)
        e.request("goto", line=2, column=5)
        self.assertTrue(e.snapshot()["navigation"]["can_go_back"])
        e.request("navigate_back")
        state = e.snapshot()
        self.assertEqual((state["line"], state["column"]), (1, 1))
        self.assertTrue(state["navigation"]["can_go_forward"])
        e.request("navigate_forward")
        state = e.snapshot()
        self.assertEqual((state["line"], state["column"]), (2, 5))
        # Result clicks in an already-open file must add their origin, too.
        e.request("open", path=str(e.file), line=1, column=3)
        e.request("navigate_back")
        state = e.snapshot()
        self.assertEqual((state["line"], state["column"]), (2, 5))
        e.request("open", path=str(e.other), line=1, column=4)
        e.request("navigate_back")
        state = e.snapshot()
        self.assertEqual((state["file"], state["line"], state["column"]), (str(e.file), 2, 5))
        # A physical Vim Ctrl-I consumes the same forward history as the GUI.
        e.keys(b"\x09")
        e.state_where(lambda s: s["file"] == str(e.other) and s["column"] == 4)
        e.keys(b"\x0f")
        e.state_where(lambda s: s["file"] == str(e.file) and s["column"] == 5)
        e.request("navigate_forward")
        self.assertEqual(e.snapshot()["file"], str(e.other))

    def test_navigation_obeys_window_history_and_unsaved_buffer_checks(self):
        e = self.editor()
        e.ex("clearjumps")
        e.request("goto", line=2, column=5)
        e.request("split", vertical=True)
        e.ex("clearjumps")
        self.assertFalse(e.snapshot()["navigation"]["can_go_back"])
        e.ex("wincmd p")
        self.assertTrue(e.snapshot()["navigation"]["can_go_back"])
        e.ex("only")
        e.request("open", path=str(e.other))
        e.keys(b"iKEEP \x1b")
        e.state_where(lambda s: s["modified"] and s["mode"] == "n")
        e.request("navigate_back", ok=False)
        state = e.snapshot()
        self.assertEqual(state["file"], str(e.other))
        self.assertTrue(state["modified"])
        e.request("save")
        self.assertTrue(e.other.read_text().startswith("KEEP "))
        e.request("navigate_back")
        self.assertEqual(e.snapshot()["file"], str(e.file))

    def test_save_and_build_uses_saved_contents_and_write_errors_prevent_launch(self):
        e = self.editor()
        e.keys(b"inew source \x1b")
        e.state_where(lambda s: s["modified"] and s["mode"] == "n")
        # Plain Build deliberately continues to see the on-disk version.
        e.request("build", command="cat main.txt > before.txt")
        e.state_where(lambda s: not s["build"]["running"] and s["build"]["exit_code"] == 0)
        self.assertEqual((self.root / "before.txt").read_text(), "original line\nsecond line\n")
        e.request("save_and_build", command="cat main.txt > after.txt")
        e.state_where(lambda s: not s["build"]["running"] and s["build"]["command"] == "cat main.txt > after.txt")
        self.assertTrue((self.root / "after.txt").read_text().startswith("new source original"))
        self.assertFalse(e.snapshot()["modified"])
        e.keys(b"ifailed save \x1b")
        e.state_where(lambda s: s["modified"] and s["mode"] == "n")
        e.ex("set readonly")
        e.request("save_and_build", ok=False, command="touch SHOULD_NOT_BUILD")
        self.assertTrue(e.snapshot()["modified"])
        self.assertFalse((self.root / "SHOULD_NOT_BUILD").exists())
        e.ex("set noreadonly")
        # Validation fails before writing when the command itself is invalid.
        before = e.file.read_text()
        e.request("save_and_build", ok=False, command=["invalid"])
        self.assertEqual(e.file.read_text(), before)
        e.ex("autocmd BufWritePre * throw 'write hook rejected'")
        e.request("save_and_build", ok=False, command="touch SHOULD_NOT_BUILD")
        self.assertFalse((self.root / "SHOULD_NOT_BUILD").exists())

    def test_new_actions_preserve_pending_vim_operator(self):
        e = self.editor()
        current_id = e.snapshot()["buffers"][0]["id"]
        e.keys("d")
        e.state_where(lambda s: s["mode"].startswith("no"))
        for action, params in [
            ("goto", {"line": 2}), ("split", {"vertical": True}),
            ("close_buffer", {"buffer": current_id}), ("next_buffer", {}),
            ("save_and_build", {"command": "touch SHOULD_NOT_BUILD"}),
            ("navigate_back", {}), ("navigate_forward", {}),
        ]:
            e.request(action, ok=False, **params)
        e.keys("w")
        e.state_where(lambda s: s["modified"] and s["mode"] == "n")
        e.request("save")
        self.assertTrue(e.file.read_text().startswith("line\n"))
        self.assertFalse((self.root / "SHOULD_NOT_BUILD").exists())

    def test_current_close_follows_runtime_navigation_without_a_snapshot_barrier(self):
        e = self.editor()
        first_id = e.snapshot()["buffers"][0]["id"]
        e.request("open", path=str(e.other))
        other_id = next(b["id"] for b in e.snapshot()["buffers"] if b["current"])
        e.request("previous_buffer")
        # Send both commands together, without allowing a frontend snapshot to
        # catch up. Closing must target the buffer selected by next_buffer.
        first_request = e.next_id
        e.next_id += 2
        e.connection.sendall((json.dumps({"id": first_request, "action": "next_buffer"}) + "\n"
            + json.dumps({"id": first_request + 1, "action": "close_current_buffer"}) + "\n").encode())
        for request_id in [first_request, first_request + 1]:
            reply = e.wait(lambda m: m.get("event") == "response" and m.get("id") == request_id)
            self.assertTrue(reply["ok"], reply)
        state = e.snapshot()
        self.assertNotIn(other_id, [b["id"] for b in state["buffers"]])
        self.assertIn(first_id, [b["id"] for b in state["buffers"]])
        self.assertEqual(state["file"], str(e.file))
        e.keys(b"iKEEP \x1b")
        e.state_where(lambda s: s["mode"] == "n" and s["modified"])
        e.request("close_current_buffer", ok=False)
        self.assertTrue(e.snapshot()["modified"])

    def prepare_ambiguous_escape(self, e: Editor, timeout_ms: int):
        e.ex(f"set timeout timeoutlen={timeout_ms} ttimeout ttimeoutlen={timeout_ms}")
        e.ex("inoremap <Esc>z <Esc>")
        # Observe Vim's real state through its normal plugin/timer mechanism,
        # so the regression proves that Save arrives during prefix decoding.
        e.ex("call timer_start(10, {-> writefile([mode(1), state('moaSw')], 'busy.marker')}, {'repeat': -1})")
        e.keys(b"iDEFERRED ")
        e.state_where(lambda s: s["mode"] == "i" and s["modified"])
        e.keys(b"\x1b")
        marker = self.root / "busy.marker"
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            if marker.exists() and "m" in marker.read_text():
                return
            e.pump(0.01)
        self.fail("Vim did not enter an ambiguous Escape mapping state")

    def test_immediate_save_waits_for_safe_state_and_keeps_prior_typed_text(self):
        e = self.editor()
        self.prepare_ambiguous_escape(e, 700)
        request_id = e.next_id
        e.next_id += 1
        e.send({"id": request_id, "action": "save_and_build", "command": "cat main.txt > saved-build.txt"})
        e.state_where(lambda s: s["waiting_for_input"])
        self.assertEqual(e.file.read_text(), "original line\nsecond line\n")
        # Native navigation must not overtake a deferred save.
        e.request("open", ok=False, path=str(e.other))
        reply = e.wait(lambda m: m.get("event") == "response" and m.get("id") == request_id)
        self.assertTrue(reply["ok"], reply)
        state = e.state_where(lambda s: not s["waiting_for_input"] and s["build"]["exit_code"] == 0)
        self.assertFalse(state["modified"])
        self.assertEqual(e.file.read_text(), "DEFERRED original line\nsecond line\n")
        self.assertEqual((self.root / "saved-build.txt").read_text(), e.file.read_text())

    def test_deferred_save_expires_without_late_replay(self):
        e = self.editor()
        self.prepare_ambiguous_escape(e, 5000)
        request_id = e.next_id
        e.next_id += 1
        e.send({"id": request_id, "action": "save"})
        e.state_where(lambda s: s["waiting_for_input"])
        reply = e.wait(lambda m: m.get("event") == "response" and m.get("id") == request_id)
        self.assertFalse(reply["ok"])
        self.assertIn("save again", reply["error"])
        e.keys("z")  # Complete the mapping after the request has expired.
        e.state_where(lambda s: s["mode"] == "n" and not s["waiting_for_input"])
        self.assertEqual(e.file.read_text(), "original line\nsecond line\n")
        self.assertTrue(e.snapshot()["modified"])
        e.request("save")
        self.assertTrue(e.file.read_text().startswith("DEFERRED original"))

    def test_async_build_diagnostics_and_secret_is_not_inherited(self):
        e = self.editor()
        command = 'if [ -n "$JTERM_VIM9_TOKEN" ]; then exit 99; fi; printf "%s\\n" "main.txt:2:3:compiler error"; exit 2'
        e.request("build", command=command)
        state = e.state_where(lambda s: not s["build"]["running"] and s["build"]["exit_code"] == 2)
        self.assertEqual(len(state["diagnostics"]), 1)
        issue = state["diagnostics"][0]
        self.assertEqual((Path(issue["filename"]).name, issue["line"], issue["column"]), ("main.txt", 2, 3))
        self.assertEqual(issue["text"], "compiler error")
        self.assertIn("main.txt:2:3:compiler error", state["build"]["output"])
        e.request("build", command="sleep 10")
        e.state_where(lambda s: s["build"]["running"])
        e.request("build", ok=False, command="echo overlap")
        e.request("cancel_build")
        state = e.state_where(lambda s: not s["build"]["running"] and s["build"]["cancelled"])
        self.assertIsNotNone(state["build"]["exit_code"])

    def test_large_build_keeps_the_latest_output_after_quickfix_capture_is_full(self):
        e = self.editor()
        e.request("build", command="i=0; while [ \"$i\" -lt 10012 ]; do printf 'output-%s\\n' \"$i\"; i=$((i+1)); done")
        state = e.state_where(lambda s: not s["build"]["running"] and s["build"]["exit_code"] == 0, timeout=15)
        self.assertEqual(len(state["build"]["output"]), 80)
        self.assertEqual(state["build"]["output"][0], "output-9932")
        self.assertEqual(state["build"]["output"][-1], "output-10011")
        self.assertEqual(state["build"]["truncated"], 9932)

    @unittest.skipUnless(shutil.which("rg"), "Install ripgrep for project search integration")
    def test_async_literal_search_and_no_shell_interpretation(self):
        e = self.editor()
        query = "needle;$(touch SEARCH_PWNED)"
        target = self.root / "a file.txt"
        target.write_text("first\n" + query + "\n", encoding="utf-8")
        e.request("search", query=query)
        state = e.state_where(lambda s: not s["search"]["running"] and s["search"]["count"] == 1)
        self.assertEqual(state["search"]["exit_code"], 0)
        self.assertEqual(state["diagnostics"][0]["filename"], str(target))
        self.assertEqual(state["diagnostics"][0]["line"], 2)
        self.assertFalse((self.root / "SEARCH_PWNED").exists())
        e.request("search", query="--definitely-not-an-rg-option")
        state = e.state_where(lambda s: s["search"]["query"].startswith("--definitely") and not s["search"]["running"])
        self.assertEqual(state["search"]["exit_code"], 1)
        self.assertEqual(state["diagnostics"], [])
        (self.root / "many.txt").write_text("bounded-search-result\n" * 300)
        e.request("search", query="bounded-search-result")
        state = e.state_where(lambda s: s["search"]["query"] == "bounded-search-result" and not s["search"]["running"])
        self.assertEqual(len(state["diagnostics"]), 256)
        self.assertIs(state["diagnostics_truncated"], True)
        self.assertEqual(state["diagnostics_omitted"], 44)

    def test_async_search_failure_is_a_visible_error_event(self):
        e = self.editor(failing_rg=True)
        e.request("search", query="anything")
        failure = e.wait(lambda m: m.get("event") == "error")
        self.assertIn("test search process failure", failure["error"])
        state = e.state_where(lambda s: not s["search"]["running"] and s["search"]["exit_code"] == 2)
        self.assertEqual(state["search"]["count"], 0)

    @unittest.skipUnless(shutil.which("rg"), "Install ripgrep for project search integration")
    def test_search_options_case_word_regex_and_literal_file_glob(self):
        e = self.editor()
        target = self.root / "search target.rs"
        target.write_text("Needle\nneedle\nneedlework\nneedle_2\nfoo.42\nfooX42\n", encoding="utf-8")
        (self.root / "other-source.txt").write_text("needle\n", encoding="utf-8")
        base = {"case": "smart", "regex": False, "whole_word": False, "glob": "*.rs"}
        for query, changes, expected in [
            ("needle", {}, 4), ("needle", {"case": "sensitive"}, 3),
            ("NEEDLE", {"case": "ignore"}, 4), ("Needle", {}, 1),
            ("needle", {"whole_word": True}, 2),
            ("foo.42", {}, 1), ("foo.42", {"regex": True}, 2),
            ("needle", {"glob": "search target.rs"}, 4),
        ]:
            with self.subTest(query=query, changes=changes):
                options = base | changes
                e.request("search_with_options", query=query, options=options)
                state = e.state_where(lambda s: s["search"]["query"] == query
                                      and s["search"]["options"] == options and not s["search"]["running"])
                self.assertEqual(state["search"]["count"], expected)
                self.assertEqual(state["quickfix"]["source"], "search")
                self.assertEqual(state["quickfix"]["count"], expected)
                self.assertTrue(all(d["filename"] == str(target) for d in state["diagnostics"]))
        # Glob shell characters remain one literal argument.
        malicious = "*.rs;$(touch GLOB_PWNED)"
        e.request("search_with_options", query="needle", options=base | {"glob": malicious})
        e.state_where(lambda s: s["search"]["options"]["glob"] == malicious and not s["search"]["running"])
        self.assertFalse((self.root / "GLOB_PWNED").exists())

    @unittest.skipUnless(shutil.which("rg"), "Install ripgrep for project search integration")
    def test_search_errors_keep_quickfix_source_and_its_omission_count(self):
        e = self.editor()
        (self.root / "many.txt").write_text("bounded-result\n" * 300)
        e.request("search", query="bounded-result")
        state = e.state_where(lambda s: s["search"]["query"] == "bounded-result" and not s["search"]["running"])
        self.assertEqual(state["diagnostics_omitted"], 44)
        original = state["quickfix"].copy()
        e.request("search_with_options", query="[", options={"regex": True})
        state = e.state_where(lambda s: s["search"]["query"] == "[" and not s["search"]["running"])
        self.assertEqual(state["search"]["exit_code"], 2)
        self.assertEqual(state["quickfix"], original)
        self.assertEqual(state["diagnostics_omitted"], 44)
        for options in [{"case": "invalid"}, {"regex": "yes"}, {"whole_word": 1},
                        {"glob": ["*.rs"]}, {"glob": "a\nb"}, []]:
            e.request("search_with_options", query="valid", options=options, ok=False)
        self.assertEqual(e.snapshot()["quickfix"], original)
        e.request("build", command="printf 'main.txt:2:4:build problem\\n'")
        state = e.state_where(lambda s: s["build"]["exit_code"] == 0 and not s["build"]["running"])
        self.assertEqual(state["quickfix"]["source"], "build")
        self.assertEqual(state["quickfix"]["count"], 1)
        self.assertEqual(state["diagnostics_omitted"], 0)
        # A user's Vim list remains explicitly identified as ordinary quickfix.
        e.ex("call setqflist([], ' ', {'title': 'User list', 'items': []})")
        self.assertEqual(e.snapshot()["quickfix"]["source"], "quickfix")

    def test_cancel_search_preserves_results_and_discards_late_callbacks(self):
        # Make job cancellation and replacement deterministic without a huge
        # filesystem scan. These real subprocesses still use Vim's job API.
        script = (
            "#!/usr/bin/python3\nimport json, signal, sys, time\n"
            "query = sys.argv[-2]\n"
            "if query == 'slow':\n"
            "    signal.signal(signal.SIGTERM, lambda *_: None)\n"
            "    time.sleep(0.5)\n"
            "print(json.dumps({'type': 'match', 'data': {'path': {'text': 'main.txt'}, "
            "'line_number': 1, 'lines': {'text': query}, 'submatches': [{'start': 0}]}}), flush=True)\n"
        )
        e = self.editor(rg_script=script)
        e.request("search", query="kept")
        state = e.state_where(lambda s: s["search"]["query"] == "kept" and not s["search"]["running"])
        original = state["quickfix"].copy()
        e.request("search", query="slow")
        e.state_where(lambda s: s["search"]["query"] == "slow" and s["search"]["running"])
        e.request("cancel_search")
        state = e.snapshot()
        self.assertFalse(state["search"]["running"])
        self.assertTrue(state["search"]["cancelled"])
        self.assertEqual(state["quickfix"], original)
        e.request("search", query="new")
        e.state_where(lambda s: s["search"]["query"] == "new" and not s["search"]["running"])
        deadline = time.monotonic() + 0.8
        while time.monotonic() < deadline:
            e.pump(0.05)
        state = e.snapshot()
        self.assertFalse(state["search"]["cancelled"])
        self.assertEqual(state["quickfix"]["title"], "Search: new")
        self.assertEqual([d["text"] for d in state["diagnostics"]], ["new"])

    @unittest.skipUnless(shutil.which("rg"), "Install ripgrep for project search integration")
    def test_search_explicit_options_ignore_rg_global_configuration(self):
        config = self.root / "ripgrep-config"
        config.write_text("--glob=!*.rs\n--case-sensitive\n")
        e = self.editor(extra_env={"RIPGREP_CONFIG_PATH": str(config)})
        (self.root / "source.rs").write_text("Needle\n")
        e.request("search_with_options", query="needle", options={"glob": "*.rs", "case": "ignore"})
        state = e.state_where(lambda s: s["search"]["query"] == "needle" and not s["search"]["running"])
        self.assertEqual(state["search"]["count"], 1)

    def test_provider_capabilities_and_actual_vim_commands(self):
        e = self.editor(plugin=True)
        self.assertTrue({"definition", "hover", "references", "rename", "format"}.issubset(e.state["capabilities"]))
        e.request("hover")
        self.assertEqual((self.root / "hover.marker").read_text(), "hover\n")
        e.request("rename", name="newSymbol")
        self.assertEqual((self.root / "rename.marker").read_text(), "newSymbol\n")
        e.request("rename", ok=False, name="bad | call writefile(['bad'], 'PWNED')")
        self.assertFalse((self.root / "PWNED").exists())
        e.request("definition")
        self.assertEqual(e.snapshot()["line"], 2)
        e.request("references")
        e.state_where(lambda s: any(d["text"] == "reference" for d in s["diagnostics"]))
        e.request("format")
        e.request("save")
        self.assertTrue(e.file.read_text().startswith("formatted by provider\n"))

    def test_missing_provider_reports_unavailable(self):
        e = self.editor()
        self.assertNotIn("hover", e.state["capabilities"])
        result = e.request("hover", ok=False)
        self.assertIn("configured Vim LSP provider", result["error"])

    def test_diagnostic_bounds_and_bool_truncation(self):
        e = self.editor()
        e.keys(":call setqflist(map(range(1, 300), {_, n -> {'filename': 'main.txt', 'lnum': n, 'col': 1, 'text': 'issue ' . n, 'type': 'E'}}))\r")
        state = e.state_where(lambda s: s["diagnostics_truncated"])
        self.assertIs(state["diagnostics_truncated"], True)
        self.assertEqual(len(state["diagnostics"]), 256)
        self.assertEqual(state["diagnostics_omitted"], 44)
        self.assertTrue(all(Path(d["filename"]).is_absolute() for d in state["diagnostics"]))

    def test_async_build_keeps_its_working_directory(self):
        e = self.editor()
        nested = self.root / "nested"
        nested.mkdir()
        e.request("build", command='sleep 0.2; printf "%s\\n" "main.txt:2:2:project-root error"; exit 1')
        e.keys(":lcd nested\r")
        state = e.state_where(lambda s: not s["build"]["running"] and s["build"]["exit_code"] == 1)
        self.assertEqual(state["cwd"], str(nested))
        self.assertEqual(state["diagnostics"][0]["filename"], str(e.file))


if __name__ == "__main__":
    unittest.main(verbosity=2)
