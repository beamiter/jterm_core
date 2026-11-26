#!/usr/bin/env python3
"""Check native GTK panes with one shared reader, writer and child lifecycle."""
import argparse
import json
import os
from pathlib import Path
import tempfile

from native import (
    Atspi, accessible_text, application_nodes, start_accessibility, wait_for, window_has_solid_color,
)
from native_jsh import application, window_for


SHELL = r"""#!/usr/bin/python3
import os
from pathlib import Path
import select
import signal
import time
import tty

root = Path(__file__).parent
tty.setraw(0)
(root / "leader").write_text(str(os.getpid()))
os.write(1, b"\x1b]133;A\x07$ \x1b]133;B\x07\r\n"
            b"\x1b]133;C;cmdline_url=prepared-probe\x07"
            b"\x1b[48;2;18;227;113mprepared-pty-ready\x1b[0m\r\n\x1b[5n")
if not select.select([0], [], [], 8)[0]:
    raise RuntimeError("no device status reply")
reply = bytearray()
while len(reply) < 4 and select.select([0], [], [], 2)[0]:
    reply.extend(os.read(0, 256))
(root / "reply").write_bytes(reply)

descendant = os.fork()
if descendant == 0:
    os.setpgid(0, 0)
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    signal.alarm(25)
    (root / "descendant").write_text(str(os.getpid()))
    while True:
        signal.pause()

deadline = time.monotonic() + 12
while time.monotonic() < deadline and not (root / "finish").exists():
    time.sleep(0.02)
os.write(1, b"prepared-pty-tail\r\n\x1b]133;D;19\x07")
os._exit(19)
"""


def thread_names(pid):
    names = []
    for task in Path(f"/proc/{pid}/task").glob("*/comm"):
        try:
            names.append(task.read_text().strip())
        except (FileNotFoundError, ProcessLookupError):
            continue
    return names


def stopped(pid):
    try:
        return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0] == "Z"
    except (FileNotFoundError, ProcessLookupError):
        return True


def run(app, binary, mode, root, *, overrides=False, legacy=False):
    root.mkdir(mode=0o700)
    shell = root / "shell"
    shell.write_text(SHELL)
    shell.chmod(0o700)
    config = root / f"{app}.toml"
    config.write_text(f'shell = {json.dumps(str(shell))}\nterminal_mode = "{mode}"\n')
    config.chmod(0o600)
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("JSH_", "JTERM_SHARED_")) and key != "BASH_ENV"}
    env.pop(f"{app.upper()}_SHELL", None)
    for key in ("HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME",
                "XDG_RUNTIME_DIR"):
        folder = root / key
        folder.mkdir(mode=0o700)
        env[key] = str(folder)
    env.update(GDK_BACKEND="x11", GTK_A11Y="atspi", GTK_IM_MODULE="simple", GSK_RENDERER="cairo")
    if overrides:
        env.update(JTERM_SHARED_RUNTIME="0", JTERM_SHARED_TRANSCRIPT="0",
                   JTERM_SHARED_ACTIVE_TERMINAL="0")
    with application(binary, config, env, root / "app.log", restore=False) as process:
        def shared_screen():
            return next((node for node in application_nodes(process.pid)
                         if node.get_state_set().contains(Atspi.StateType.SHOWING)
                         and node.get_description() == "Shared active terminal"), None)

        if legacy:
            # Older libvte builds can draw the terminal without an AT-SPI
            # terminal node. Require pixels produced by this exact PTY.
            window = window_for(process.pid)
            wait_for("compatibility PTY output painted",
                     lambda: window_has_solid_color(window, (18, 227, 113)))
            assert shared_screen() is None
        else:
            wait_for("early shared PTY output painted", lambda: (
                "prepared-pty-ready" in accessible_text(shared_screen())))
        reply = root / "reply"
        wait_for("device status reply", lambda: reply.is_file() and len(reply.read_bytes()) >= 4)
        assert reply.read_bytes() == b"\x1b[0n", reply.read_bytes()
        leader = int((root / "leader").read_text())
        child = root / "descendant"
        wait_for("separate background group", lambda: child.is_file() and child.read_text().isdigit())
        descendant = int(child.read_text())
        assert os.getpgid(descendant) == descendant != os.getpgid(leader)
        names = thread_names(process.pid)
        shared_workers = [name for name in names if name in {"pty-reader", "pty-writer"}]
        legacy_workers = [name for name in names if name.startswith(f"{app}-pty-")]
        if legacy:
            assert not shared_workers and len(legacy_workers) == 2, names
            workers = set(legacy_workers)
        else:
            assert sorted(shared_workers) == ["pty-reader", "pty-writer"] and not legacy_workers, names
            workers = set(shared_workers)

        (root / "finish").touch()
        wait_for("leader reaped", lambda: not Path(f"/proc/{leader}").exists())
        wait_for("separate background group stopped", lambda: stopped(descendant))
        wait_for("PTY workers stopped", lambda: workers.isdisjoint(thread_names(process.pid)))
    backend = "legacy oracle" if legacy else "shared with old overrides" if overrides else "default shared"
    print(f"{app} {mode} {backend} worker ownership, PTY reply and child cleanup passed", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", choices=("forge", "anvil"))
    parser.add_argument("binary", type=Path)
    parser.add_argument("--legacy-binary", type=Path,
                        help="optional separate binary built with the old backend for comparison")
    parser.add_argument("--mode", choices=("block", "chat", "unified"), action="append")
    args = parser.parse_args()
    start_accessibility()
    with tempfile.TemporaryDirectory(prefix="gtk-prepared-pty-") as directory:
        binary = args.binary.resolve(strict=True)
        legacy = args.legacy_binary.resolve(strict=True) if args.legacy_binary else None
        for mode in args.mode or ("block", "chat", "unified"):
            run(args.app, binary, mode, Path(directory) / mode)
            run(args.app, binary, mode, Path(directory) / f"{mode}-old-overrides", overrides=True)
            if legacy is not None:
                run(args.app, legacy, mode, Path(directory) / f"{mode}-legacy", overrides=True, legacy=True)


if __name__ == "__main__":
    main()
