#!/usr/bin/env python3
"""Verify pointer selection across a completed card and its running terminal."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

from native import Atspi, accessible_text, application_nodes, start_accessibility, wait_for
from native_jsh import application, xdotool

ARCHIVED = "archived-cross-copy witness"
LIVE = "live-cross-copy witness"
LATE = "released-after-copy"
SENTINEL = b"selection test clipboard sentinel"
SHELL = r"""#!/usr/bin/python3
import os
from pathlib import Path
import select
import time
import tty

root = Path(__file__).parent
(root / "leader").write_text(str(os.getpid()))
tty.setraw(0)
os.write(1, b"\x1b]133;A\x07$ \x1b]133;B\x07\r\n\x1b]133;C;cmdline_url=archived\x07archived-cross-copy witness\r\n")
os.write(1, b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07\r\n\x1b]133;C;cmdline_url=running\x07live-cross-copy witness\r\n")
sent = False
deadline = time.monotonic() + 45
while time.monotonic() < deadline:
    if not sent and (root / "emit-after-selection").exists():
        os.write(1, b"released-after-copy\r\n")
        sent = True
    if select.select([0], [], [], 0.025)[0] and not os.read(0, 1):
        break
"""


def clipboard(*args, **kwargs):
    return subprocess.run(
        ["xclip", "-selection", "clipboard", *args], check=True, timeout=5,
        stdout=subprocess.DEVNULL if "-i" in args else subprocess.PIPE,
        stderr=subprocess.DEVNULL, **kwargs,
    ).stdout


def point(node, substring):
    text = node.get_text_iface()
    start = accessible_text(node).index(substring)
    rect = Atspi.Text.get_character_extents(text, start, Atspi.CoordType.WINDOW)
    assert rect.x >= 0 and rect.y >= 0 and rect.width > 0 and rect.height > 0, rect
    return rect.x + max(1, rect.width // 2), rect.y + rect.height // 2


def drag(window, start, end):
    xdotool("mousemove", "--window", window, str(start[0]), str(start[1]))
    xdotool("mousedown", "1")
    try:
        for step in range(1, 13):
            delta = [str(a + (b - a) * step // 12) for a, b in zip(start, end)]
            xdotool("mousemove", "--window", window, *delta, "sleep", "0.03")
    finally:
        xdotool("mouseup", "1")


def copy():
    xdotool("key", "--clearmodifiers", "ctrl+shift+c")

    def changed():
        value = clipboard("-o")
        return value if value != SENTINEL else None

    return wait_for("native clipboard after Ctrl+Shift+C", changed).decode()


def exercise(binary, app, mode, root):
    root.mkdir(mode=0o700)
    shell = root / "shell"
    shell.write_text(SHELL)
    shell.chmod(0o700)
    config = root / f"{app}.toml"
    config.write_text(f'shell = {json.dumps(str(shell))}\nterminal_mode = "{mode}"\n')
    config.chmod(0o600)
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("JSH_", "JTERM_SHARED_"))
           and key not in (f"{app.upper()}_SHELL", "BASH_ENV")}
    for key in ("HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR"):
        directory = root / key
        directory.mkdir(mode=0o700)
        env[key] = str(directory)
    env.update(GDK_BACKEND="x11", GSK_RENDERER="cairo", GTK_A11Y="atspi", GTK_IM_MODULE="simple",
               JTERM_SHARED_ACTIVE_TERMINAL="1")

    with application(binary, config, env, root / f"{app}.log", restore=False) as process:
        pid = process.pid
        window = xdotool("search", "--onlyvisible", "--pid", str(pid)).split()[0]

        def shown():
            return [node for node in application_nodes(pid)
                    if node.get_state_set().contains(Atspi.StateType.SHOWING)]

        def surface(marker, live=False):
            return next((node for node in shown()
                         if node.get_role() == Atspi.Role.TEXT
                         and (node.get_description() == "Shared active terminal") == live
                         and marker in accessible_text(node)), None)

        archived = wait_for("mapped archived output card", lambda: surface(ARCHIVED))
        live = wait_for("mapped running terminal", lambda: surface(LIVE, live=True))
        child = int((root / "leader").read_text())
        tty = Path(f"/proc/{child}/fd/0")
        source = tty.readlink()
        assert str(source).startswith("/dev/pts/"), source
        origin = point(archived, "cross-copy witness")
        target = point(live, "cross-copy witness")
        expected = f"{ARCHIVED}\n$ \n{LIVE}\n"
        for start, end in ((origin, target), (target, origin)):
            clipboard("-i", input=SENTINEL)
            drag(window, start, end)
            copied = copy()
            assert copied == expected, (mode, "whole-surface order and scope", copied)

        clipboard("-i", input=SENTINEL)
        drag(window, origin, target)
        (root / "emit-after-selection").touch()
        wait_for("paused output badge", lambda: any(
            "output paused" in node.get_name().lower()
            for node in shown() if node.get_name()))
        assert LATE not in accessible_text(surface(LIVE, live=True))
        assert copy() == expected
        wait_for("parked PTY output replay after the copy", lambda: surface(LATE, live=True))
        assert tty.readlink() == source

    print(f"{app} {mode} cross selection passed: both drag directions, exact clipboard, held output and same PTY")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", choices=("forge", "anvil"))
    parser.add_argument("binary", type=Path)
    parser.add_argument("--mode", choices=("block", "chat"), action="append")
    args = parser.parse_args()
    start_accessibility()
    with tempfile.TemporaryDirectory(prefix="native-cross-selection-") as directory:
        for mode in args.mode or ("block", "chat"):
            exercise(args.binary.resolve(strict=True), args.app, mode, Path(directory) / mode)


if __name__ == "__main__":
    main()
