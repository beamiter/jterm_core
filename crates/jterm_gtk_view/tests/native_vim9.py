#!/usr/bin/env python3
"""Verify the native buffer and compatibility view use the same real Vim PTY."""

import argparse
import fcntl
import os
from pathlib import Path
import re
import shlex
import struct
import subprocess
import tempfile
import termios
import threading

from native import Atspi, accessible_text, application_nodes, first_named, start_accessibility, wait_for


def record_vim_geometry(app_pid, stop, sizes):
    children = Path(f"/proc/{app_pid}/task/{app_pid}/children")
    while not stop.wait(0.004):
        try:
            pids = children.read_text().split()
        except OSError:
            return
        for pid in pids:
            process = Path(f"/proc/{pid}")
            try:
                if not (process / "comm").read_text().startswith("vim"):
                    continue
                fd = os.open(process / "fd/0", os.O_RDONLY | os.O_NOCTTY | os.O_NONBLOCK)
                try:
                    size = struct.unpack("HHHH", fcntl.ioctl(fd, termios.TIOCGWINSZ, bytes(8)))[:2]
                finally:
                    os.close(fd)
            except OSError:
                continue
            sizes.add(size)


def run(app, binary, root, native_buffer, embedded=False):
    project = root / "Vim project"
    project.mkdir()
    document = project / "edit.txt"
    document.write_text("vim9-original\nsecond line\n", encoding="utf-8")
    config = root / f"{app}.toml"
    config.write_text('shell = "/bin/sh"\nterminal_mode = "vte"\n'
                      f"[vim9]\nnative_buffer = {str(native_buffer).lower()}\nstatus_bar = true\nauto_gui = true\n")
    config.chmod(0o600)
    environment = os.environ.copy()
    for name in ("HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR"):
        directory = root / name
        directory.mkdir(mode=0o700)
        environment[name] = str(directory)
    environment.update(GDK_BACKEND="x11", GSK_RENDERER="cairo", GTK_A11Y="atspi")
    environment.pop("JTERM_SHARED_RUNTIME", None)
    with (root / "app.log").open("w+") as log:
        target = ["--no-restore", str(project)] if embedded else ["--ide", str(document)]
        process = subprocess.Popen([binary, "--config", str(config), *target],
                                   env=environment, stdout=log, stderr=log)
        stop = threading.Event()
        sizes = set()
        observer = threading.Thread(target=record_vim_geometry, args=(process.pid, stop, sizes))
        observer.start()
        try:
            outcome = exercise(app, process, document, native_buffer, embedded)
            stop.set()
            observer.join(timeout=2)
            assert sizes and all(rows > 1 and columns > 1 for rows, columns in sizes), sorted(sizes)
            print(f"{outcome} (PTY geometries {sorted(sizes)})")
        except Exception:
            log.seek(0)
            print(log.read())
            print("Vim PTY geometries:", sorted(sizes))
            if process.poll() is None:
                for node in application_nodes(process.pid):
                    name = node.get_name() or ""
                    if name.startswith(("GUI · ", "Vim · ", "Starting Vim 9")):
                        print("Vim9 status:", name)
                    if name == "Vim9 editor" and node.get_text_iface() is not None:
                        content = [line.rstrip() for line in accessible_text(node).splitlines() if line.strip()]
                        print("Vim9 compatibility screen:", content[:8])
            raise
        finally:
            stop.set()
            observer.join(timeout=2)
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def exercise(app, process, document, native_buffer, embedded):
    def named(name, state=Atspi.StateType.SHOWING):
        return first_named(process.pid, name, state)

    def focus():
        return named("Vim9 editor", Atspi.StateType.FOCUSED)

    def tool(*args):
        return subprocess.check_output(["xdotool", *args], text=True, stderr=subprocess.DEVNULL, timeout=5)

    def command(text):
        tool("type", "--clearmodifiers", "--delay", "12", text)
        tool("key", "Return")

    def shell():
        return named("Shared terminal screen", Atspi.StateType.FOCUSED)

    window = wait_for("Vim9 workbench window", lambda: tool("search", "--onlyvisible", "--pid", str(process.pid)).split()[0])
    tool("windowfocus", "--sync", window)
    if embedded:
        wait_for("focused shared shell before Vim", shell)
        command("vim " + shlex.quote(str(document)) + r'''; printf 'vim-return=[%s]\n' "$?"''')
    editor = wait_for("focused Vim9 editor", focus)
    assert editor.get_description() == "Shared runtime session", editor.get_description()
    display = "GUI · RGB / Pango" if native_buffer else "Vim · compatibility display"
    wait_for(display, lambda: named(display))
    wait_for("real Vim file buffer", lambda: "vim9-original" in accessible_text(focus()))

    command(":echom 'vim9-command-line-witness'")
    wait_for("Vim executed its command-line input", lambda: "vim9-command-line-witness" in accessible_text(focus()))
    tool("key", "Escape", "g", "g", "0", "i")
    tool("type", "--clearmodifiers", "--delay", "12", "shared-")
    tool("key", "Escape")
    command(":wq")
    if embedded:
        wait_for("the waiting shared shell received Vim's exit status", lambda: re.search(
            r"(?m)^vim-return=\[0\] *$", accessible_text(shell())))
        assert process.poll() is None
    else:
        assert process.wait(timeout=10) == 0
    assert document.read_text(encoding="utf-8") == "shared-vim9-original\nsecond line\n"
    route = "embedded" if embedded else "standalone"
    return f"{app} {route} Vim9 {display} passed: shared PTY, Vim command line, special keys, edit and save"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", choices=("forge", "anvil"))
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    start_accessibility()
    for native_buffer in (True, False):
        with tempfile.TemporaryDirectory(prefix=f"{args.app}-shared-vim9-") as directory:
            run(args.app, args.binary.resolve(strict=True), Path(directory), native_buffer)
    with tempfile.TemporaryDirectory(prefix=f"{args.app}-shared-vim9-shell-") as directory:
        run(args.app, args.binary.resolve(strict=True), Path(directory), True, embedded=True)


if __name__ == "__main__":
    main()
