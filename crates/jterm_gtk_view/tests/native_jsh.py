#!/usr/bin/env python3
"""Verify ordinary GTK jsh panes and saved shell state on an isolated X11 display."""
import argparse
from contextlib import contextmanager
import fcntl
import json
import os
from pathlib import Path
import re
import shlex
import struct
import subprocess
import tempfile
import termios

from native import (
    Atspi, accessible_text, application_nodes, first_named, start_accessibility, wait_for,
    window_has_solid_color,
)


def xdotool(*args):
    return subprocess.check_output(["xdotool", *args], text=True, timeout=5).strip()


def window_for(pid):
    return xdotool("search", "--onlyvisible", "--pid", str(pid)).split()[0]


def send(command):
    xdotool("type", "--clearmodifiers", "--delay", "5", command)
    xdotool("key", "--clearmodifiers", "Return")


def shared_screen(pid, state=Atspi.StateType.FOCUSED):
    return first_named(pid, "Shared terminal screen", state)


def wait_text(pid, text):
    line = re.compile(r"(?m)^" + re.escape(text) + r" *$")
    try:
        wait_for(text, lambda: line.search(accessible_text(shared_screen(pid))))
    except RuntimeError as error:
        last = accessible_text(shared_screen(pid, Atspi.StateType.SHOWING))
        visible = [row.rstrip() for row in last.splitlines() if row.strip()]
        raise AssertionError(f"Missing {text!r} in terminal rows {visible!r}") from error


def shells(pid, program):
    """Observe direct children with the exact configured executable and argv."""
    found = {}
    for child in Path(f"/proc/{pid}/task/{pid}/children").read_text().split():
        proc = Path(f"/proc/{child}")
        try:
            if not os.path.samefile(proc / "exe", program):
                continue
            argv = (proc / "cmdline").read_bytes().rstrip(b"\0").decode().split("\0")
        except (FileNotFoundError, ProcessLookupError):
            continue
        assert len(argv) == 3 and argv[1] == "--session", argv
        assert re.fullmatch(r"[A-Za-z0-9_-]+", argv[2]), argv
        assert argv[2] not in found, found
        found[argv[2]] = int(child)
    return found


def pty_columns(child):
    fd = os.open(f"/proc/{child}/fd/0", os.O_RDONLY | os.O_NOCTTY | os.O_NONBLOCK)
    try:
        size = fcntl.ioctl(fd, termios.TIOCGWINSZ, bytes(8))
        return struct.unpack("HHHH", size)[1]
    finally:
        os.close(fd)


@contextmanager
def application(binary, config, environment, log_path, restore, launch_args=()):
    command = [binary, "--config", str(config)]
    if not restore:
        command.append("--no-restore")
    command.extend(launch_args)
    with log_path.open("w+") as log:
        process = subprocess.Popen(command, cwd=config.parent, env=environment, stdout=log, stderr=log)
        try:
            if environment.get("GDK_BACKEND") != "wayland":
                window = wait_for("terminal window", lambda: window_for(process.pid))
                xdotool("windowfocus", "--sync", window)
            yield process
        except Exception:
            log.seek(0)
            print(log.read().strip())
            raise
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def close_window(process, confirmation=None):
    button = wait_for("Close window button", lambda: first_named(process.pid, "Close window"))
    assert Atspi.Action.do_action(button.get_action_iface(), 0)
    if confirmation:
        choice = wait_for("window closed or requested confirmation", lambda: (
            True if process.poll() is not None else next(
                (node for node in application_nodes(process.pid)
                 if node.get_name() == confirmation and node.get_role() == Atspi.Role.PUSH_BUTTON
                 and node.get_state_set().contains(Atspi.StateType.SHOWING)), None)))
        if choice is not True:
            assert Atspi.Action.do_action(choice.get_action_iface(), 0)
    try:
        wait_for("closed terminal window", lambda: process.poll() is not None)
    except RuntimeError as error:
        names = [node.get_name() for node in application_nodes(process.pid)
                 if node.get_state_set().contains(Atspi.StateType.SHOWING)]
        raise AssertionError(f"Window stayed open. Accessible labels: {[name for name in names if name][-16:]!r}") from error
    assert process.returncode == 0


def exercise(args, root):
    config = root / f"{args.app}.toml"
    config.write_text(f"shell = {json.dumps(str(args.jsh))}\nterminal_mode = \"vte\"\n")
    config.chmod(0o600)
    environment = {name: value for name, value in os.environ.items() if not name.startswith("JSH_")}
    for name in ("HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR"):
        directory = root / name
        directory.mkdir(mode=0o700)
        environment[name] = str(directory)
    environment.update(GDK_BACKEND="x11", GSK_RENDERER="cairo",
                       GTK_A11Y="atspi", GTK_IM_MODULE="simple")
    environment.pop("JTERM_SHARED_RUNTIME", None)
    environment.pop(f"{args.app.upper()}_SHELL", None)
    environment.pop("BASH_ENV", None)
    home = Path(environment["HOME"])
    (home / ".bashrc").write_text("printf 'shared-jsh-bootstrap\\n'\n")
    working = root / "parent directory outside the shell home" / "saved working directory"
    working.mkdir(parents=True)
    with application(args.binary, config, environment, root / "initial.log", restore=False) as process:
        pid = process.pid
        assert wait_for("shared jsh pane", lambda: shared_screen(pid)).get_description() == "Shared runtime session"
        wait_text(pid, "shared-jsh-bootstrap")
        original = wait_for("configured jsh child", lambda: shells(pid, args.jsh))
        assert len(original) == 1, original
        session, child = next(iter(original.items()))
        wait_text(pid, "❯")
        send("export FAMILY_RESUME_WITNESS=retained; cd "
             + shlex.quote(str(working)) + r"; printf 'jsh-state-prepared\n'")
        wait_text(pid, "jsh-state-prepared")
        columns = pty_columns(child)
        draft = root / "split-draft"
        xdotool("type", "--clearmodifiers", "--delay", "5",
                "printf 'draft retained' > " + shlex.quote(str(draft)))
        xdotool("key", "--clearmodifiers", "ctrl+shift+e")

        def split():
            current = shells(pid, args.jsh)
            return current if len(current) == 2 else None

        panes = wait_for("two jsh children with different session ids", split)
        assert panes[session] == child, panes
        wait_for("the original jsh PTY narrowed", lambda: 0 < pty_columns(child) < columns)
        wait_for("focus in the new shared split", lambda: shared_screen(pid)
                 and "jsh-state-prepared" not in accessible_text(shared_screen(pid)))
        wait_text(pid, "❯")
        send("exit")
        wait_for("the split exited and returned focus", lambda: shells(pid, args.jsh) == original)
        wait_for("the original jsh PTY recovered its width", lambda: pty_columns(child) == columns)
        wait_text(pid, "jsh-state-prepared")
        assert not draft.exists(), "resize submitted the draft"
        xdotool("key", "--clearmodifiers", "Return")
        wait_for("the user submitted the retained draft", lambda: (
            draft.is_file() and draft.read_bytes() == b"draft retained"))
        wait_text(pid, "❯")
        close_window(process)
    assert not Path(f"/proc/{child}").exists(), "the initial jsh child survived app exit"
    state = json.loads((home / ".jsh" / "sessions" / f"{session}.json").read_text())
    assert state["session_id"] == session

    with application(args.binary, config, environment, root / "restore.log", restore=True) as process:
        pid = process.pid
        assert wait_for("restored shared screen", lambda: shared_screen(pid)).get_description() == "Shared runtime session"
        restored = wait_for("restored jsh process", lambda: shells(pid, args.jsh))
        assert restored.keys() == original.keys() and restored[session] != child, restored
        wait_text(pid, "❯")
        send(r"""test "$FAMILY_RESUME_WITNESS" = retained && test "$PWD" = """
             + shlex.quote(str(working)) + r" && printf 'jsh-state-restored\n'")
        wait_text(pid, "jsh-state-restored")
        close_window(process)
    assert not Path(f"/proc/{restored[session]}").exists(), "the restored jsh child survived app exit"

    environment["JTERM_SHARED_RUNTIME"] = "0"
    with application(args.binary, config, environment, root / "legacy.log", restore=False) as process:
        pid = process.pid
        legacy = wait_for("jsh in the compatibility terminal", lambda: shells(pid, args.jsh))
        assert len(legacy) == 1, legacy
        legacy_child = next(iter(legacy.values()))
        fd = os.open(f"/proc/{legacy_child}/fd/0", os.O_RDONLY | os.O_NOCTTY | os.O_NONBLOCK)
        try:
            # exec reaches /proc before jsh enables its raw-mode input editor.
            wait_for("jsh input editor in the compatibility PTY", lambda: (
                termios.tcgetattr(fd)[3] & termios.ICANON == 0))
        finally:
            os.close(fd)
        receipt = root / "legacy-input"
        window = window_for(pid)
        send("printf 'legacy-jsh' > " + shlex.quote(str(receipt))
             + r"; printf '\033[48;2;18;227;113mlegacy-jsh\033[0m\n'")
        wait_for("input reached the compatibility PTY", lambda: (
            receipt.is_file() and receipt.read_bytes() == b"legacy-jsh"))
        wait_for("compatibility jsh output painted",
                 lambda: window_has_solid_color(window, (18, 227, 113)))
        assert shared_screen(pid, Atspi.StateType.SHOWING) is None
        close_window(process, confirmation="Close Window" if args.app == "forge" else "Close")
    wait_for("compatibility jsh child stopped", lambda: not Path(f"/proc/{legacy_child}").exists())
    print(f"{args.app} X11 jsh passed: bootstrap, split reflow and draft, session restore and rollback")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", choices=("forge", "anvil"))
    parser.add_argument("binary", type=Path)
    parser.add_argument("--jsh", type=Path, required=True)
    args = parser.parse_args()
    args.binary = args.binary.resolve(strict=True)
    args.jsh = args.jsh.absolute()
    if args.jsh.name != "jsh" or not args.jsh.is_file() or not os.access(args.jsh, os.X_OK):
        parser.error("--jsh must point to an executable named jsh")
    start_accessibility()
    with tempfile.TemporaryDirectory(prefix=f"{args.app}-shared-jsh-") as directory:
        exercise(args, Path(directory))


if __name__ == "__main__":
    main()
