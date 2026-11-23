#!/usr/bin/env python3
"""Verify ordinary GTK jsh panes and saved shell state on an isolated X11 display."""
import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile

from native import Atspi, accessible_text, application_nodes, first_named, start_accessibility, wait_for


def xdotool(*args):
    return subprocess.check_output(["xdotool", *args], text=True, timeout=5).strip()


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


@contextmanager
def application(binary, config, environment, log_path, restore):
    command = [binary, "--config", str(config)]
    if not restore:
        command.append("--no-restore")
    with log_path.open("w+") as log:
        process = subprocess.Popen(command, cwd=config.parent, env=environment, stdout=log, stderr=log)
        try:
            window = wait_for("terminal window", lambda: xdotool(
                "search", "--onlyvisible", "--pid", str(process.pid)).split()[0])
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


def close_window(process):
    button = wait_for("Close window button", lambda: first_named(process.pid, "Close window"))
    assert Atspi.Action.do_action(button.get_action_iface(), 0)
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
    environment.update(GDK_BACKEND="x11", GSK_RENDERER="cairo", GTK_A11Y="atspi", JTERM_SHARED_RUNTIME="1")
    environment.pop(f"{args.app.upper()}_SHELL", None)
    environment.pop("BASH_ENV", None)
    home = Path(environment["HOME"])
    (home / ".bashrc").write_text("printf 'shared-jsh-bootstrap\\n'\n")
    working = root / "saved working directory"
    working.mkdir()
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
        xdotool("key", "--clearmodifiers", "ctrl+shift+e")

        def split():
            current = shells(pid, args.jsh)
            return current if len(current) == 2 else None

        panes = wait_for("two jsh children with different session ids", split)
        assert panes[session] == child, panes
        wait_for("focus in the new shared split", lambda: shared_screen(pid)
                 and "jsh-state-prepared" not in accessible_text(shared_screen(pid)))
        wait_text(pid, "❯")
        send("exit")
        wait_for("the split exited and returned focus", lambda: shells(pid, args.jsh) == original)
        wait_text(pid, "jsh-state-prepared")
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

    for override in (None, "0"):
        if override is None:
            environment.pop("JTERM_SHARED_RUNTIME")
        else:
            environment["JTERM_SHARED_RUNTIME"] = override
        with application(args.binary, config, environment, root / f"legacy-{override}.log", restore=False) as process:
            pid = process.pid
            wait_for("the compatibility terminal", lambda: next(
                (node for node in application_nodes(pid) if node.get_role() == Atspi.Role.TERMINAL
                 and node.get_state_set().contains(Atspi.StateType.SHOWING)), None))
            assert shared_screen(pid, Atspi.StateType.SHOWING) is None
            wait_for("jsh in the compatibility terminal", lambda: shells(pid, args.jsh))
    print(f"{args.app} X11 jsh passed: bootstrap, independent split, same saved shell id, cwd, environment and rollback")


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
