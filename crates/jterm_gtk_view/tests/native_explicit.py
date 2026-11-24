#!/usr/bin/env python3
"""Exercise an explicit command in a shared Forge or Anvil pane."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

from native import Atspi, application_nodes, start_accessibility, wait_for
from native_jsh import application, shared_screen, wait_text, xdotool


PROBE = r"""import json, os, select, sys, tty
from pathlib import Path

def read_exact(count):
    received = b""
    while len(received) < count:
        if not select.select([0], [], [], 8)[0]:
            raise TimeoutError("waiting for terminal bytes")
        chunk = os.read(0, count - len(received))
        if not chunk:
            raise EOFError("terminal closed")
        received += chunk
    return received

assert os.isatty(0) and os.getpgrp() == os.tcgetpgrp(0)
tty.setraw(0)
os.write(1, b"\x1b[5n")
reply = read_exact(4)
with Path("launches.jsonl").open("a") as launches:
    print(json.dumps({"pid": os.getpid(), "cwd": os.getcwd(), "argv": sys.argv[1:],
                      "reply": reply.hex(), "term": os.environ["TERM"]}), file=launches)
os.write(1, b"SHARED EXPLICIT COMMAND READY\r\n")
payload = read_exact(len(b"from-shared-pane\r"))
Path("input").write_bytes(payload)
os.write(1, b"SHARED EXPLICIT COMMAND FINAL OUTPUT\r\n")
sys.exit(23)
"""


def fixture(root, app, backend):
    env = dict(os.environ, PATH="tools:/usr/bin:/bin", LC_ALL="C.UTF-8",
               GDK_BACKEND=backend, GTK_A11Y="atspi", GSK_RENDERER="cairo")
    if backend == "wayland":
        socket = Path(os.environ["XDG_RUNTIME_DIR"]) / os.environ["WAYLAND_DISPLAY"]
        if not socket.is_absolute() or not socket.is_socket():
            raise RuntimeError("WAYLAND_DISPLAY must identify the private compositor socket")
        env["WAYLAND_DISPLAY"] = str(socket)
        env.pop("DISPLAY", None)
    for variable in ("JTERM_SHARED_RUNTIME", "FLATPAK_ID", "FORGE_SAFE_MODE", f"{app.upper()}_SHELL"):
        env.pop(variable, None)
    for variable in ("HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME",
                     "XDG_STATE_HOME", "XDG_RUNTIME_DIR"):
        directory = root / variable
        directory.mkdir(mode=0o700)
        env[variable] = str(directory)
    config = root / f"{app}.toml"
    config.write_text('shell = "/bin/sh"\nterminal_mode = "vte"\n')
    config.chmod(0o600)
    project = root / "project with spaces"
    command_dir = project / "tools"
    command_dir.mkdir(parents=True)
    (project / "probe.py").write_text(PROBE)
    # The executable has no shebang. This exercises the actual ENOEXEC path.
    launcher = command_dir / "pty probe"
    launcher.write_text('exec /usr/bin/python3 probe.py "$@"\n')
    launcher.chmod(0o700)
    missing_interpreter = root / "missing interpreter"
    broken = command_dir / "broken"
    broken.write_text(f"#!{missing_interpreter}\nexit 1\n")
    broken.chmod(0o700)
    return env, config, project


def run(app, binary, root, backend, wtype):
    env, config, project = fixture(root, app, backend)
    arguments = ["--flag", "value with spaces", "", "'quotes'", "$HOME", "中文"]
    launch = ("--mode", "vte", str(project), "--execute")
    with application(binary, config, env, root / "command.log", restore=False,
                     launch_args=(*launch, "tools/pty probe", *arguments)) as process:
        view = wait_for("shared explicit terminal", lambda: shared_screen(process.pid))
        assert view.get_description() == "Shared runtime session"
        wait_text(process.pid, "SHARED EXPLICIT COMMAND READY")
        receipt, = (json.loads(line) for line in (project / "launches.jsonl").read_text().splitlines())
        assert receipt == {"pid": receipt["pid"], "cwd": str(project), "argv": arguments,
                           "reply": "1b5b306e", "term": "xterm-256color"}, receipt
        assert Path(f"/proc/{receipt['pid']}/stat").exists()
        if backend == "wayland":
            subprocess.run([wtype, "from-shared-pane", "-k", "Return"], env=env, check=True, timeout=5)
        else:
            xdotool("type", "--clearmodifiers", "from-shared-pane")
            xdotool("key", "--clearmodifiers", "Return")
        input_receipt = project / "input"
        wait_for("exact PTY input", lambda: input_receipt.is_file()
                 and input_receipt.read_bytes() == b"from-shared-pane\r")
        wait_for("explicit command reaped", lambda: not Path(f"/proc/{receipt['pid']}/stat").exists())
        assert len((project / "launches.jsonl").read_text().splitlines()) == 1

    with application(binary, config, env, root / "failed-command.log", restore=False,
                     launch_args=(*launch, "broken")) as process:
        def diagnostic():
            return next((name for node in application_nodes(process.pid)
                         if (name := node.get_name()) and name.startswith("Cannot start terminal:")
                         and node.get_state_set().contains(Atspi.StateType.SHOWING)), None)

        assert "execute PTY program" in wait_for("the shared exec failure", diagnostic)
        assert shared_screen(process.pid, Atspi.StateType.SHOWING) is None

    print(f"{app} {backend} explicit command passed: exact argv, child cwd, executable text, "
          "terminal query and input, child reaping and visible exec failure", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", choices=("forge", "anvil"))
    parser.add_argument("binary", type=Path)
    parser.add_argument("--backend", choices=("x11", "wayland"), default="x11")
    parser.add_argument("--wtype", default="wtype", help="virtual keyboard client for Wayland")
    args = parser.parse_args()
    start_accessibility()
    with tempfile.TemporaryDirectory(prefix=f"{args.app}-explicit-") as directory:
        run(args.app, args.binary.resolve(strict=True), Path(directory), args.backend, args.wtype)
