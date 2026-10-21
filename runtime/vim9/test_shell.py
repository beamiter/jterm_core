#!/usr/bin/env python3
"""The actual POSIX shim under real PTYs, with argument-recording executables."""

import json
import os
from pathlib import Path
import pty
import shutil
import subprocess
import sys
import tempfile
import unittest


class ShellVimTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim-shell-")
        self.root = Path(self.directory.name)
        self.shim = self.root / "shim"
        self.real = self.root / "original tools"
        self.shim.mkdir()
        self.real.mkdir()
        self.log = self.root / "argv.json"
        shutil.copyfile(Path(__file__).with_name("vim-shell.sh"), self.shim / "vim")
        (self.shim / "vim").chmod(0o700)
        for path, kind, status in [(self.real / "vim", "terminal", 17), (self.root / "front end", "gui", 23)]:
            path.write_text(f"#!{sys.executable}\nimport json,os,sys\n"
                            f"json.dump(dict(kind={kind!r},argv=sys.argv[1:],cwd=os.getcwd()),open(os.environ['SHIM_TEST_LOG'],'w'))\n"
                            f"sys.exit({status})\n", encoding="utf-8")
            path.chmod(0o700)
        self.env = dict(os.environ, PATH=f"{self.shim}:{self.real}:/usr/bin:/bin",
                        JTERM_VIM9_FRONTEND=str(self.root / "front end"),
                        DISPLAY=":stub", SHIM_TEST_LOG=str(self.log))
        self.env.pop("JTERM_VIM9_AUTO_GUI", None)

    def tearDown(self):
        self.directory.cleanup()

    def invoke(self, args, *, tty=True, env=None):
        master, slave = pty.openpty()
        try:
            result = subprocess.run([str(self.shim / "vim"), *args], stdin=slave,
                                    stdout=slave if tty else subprocess.PIPE, stderr=slave,
                                    cwd=self.root, env=self.env | (env or {}), timeout=5)
        finally:
            os.close(master)
            os.close(slave)
        return result.returncode, json.loads(self.log.read_text())

    def test_interactive_argv_cwd_and_status_are_exact(self):
        args = ["-d", "+12", "one space.txt", "中文'|$().rs", "--", "-es"]
        status, record = self.invoke(args)
        self.assertEqual(status, 23)
        self.assertEqual(record, dict(kind="gui", cwd=str(self.root),
                                     argv=["--vim-shell", str(self.real / "vim"), "--", *args]))
        self.assertFalse((self.root / "injected").exists())

    def test_queries_batch_pipes_and_missing_display_use_original_vim(self):
        for args in [["--version"], ["--help"], ["-es", "script.vim"], ["-nes"], ["-g"], ["-y"], ["--gui"],
                     ["--remote", "file"], ["-s", "keys"], ["-"], ["-S", "-es"],
                     ["-S", "--version"], ["-S", "-S", "-es"]]:
            with self.subTest(args=args):
                status, record = self.invoke(args)
                self.assertEqual((status, record["kind"], record["argv"]), (17, "terminal", args))
        for kwargs in [dict(tty=False), dict(env={"DISPLAY": "", "WAYLAND_DISPLAY": ""}),
                       dict(env={"JTERM_VIM9_AUTO_GUI": "0"}), dict(env={"JTERM_VIM9_FRONTEND": "/no/such/app"})]:
            status, record = self.invoke(["file"], **kwargs)
            self.assertEqual((status, record["kind"]), (17, "terminal"))

    def test_option_operands_and_repeated_shim_path_do_not_recurse(self):
        args = ["-u", "-es", "-c", "echo '--version'", "-S", "startup.vim", "file"]
        status, record = self.invoke(args, env={"PATH": f"{self.shim}:{self.shim}:{self.real}:/usr/bin:/bin"})
        self.assertEqual(status, 23)
        self.assertEqual(record["argv"][3:], args)


if __name__ == "__main__":
    unittest.main()
