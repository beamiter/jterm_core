#!/usr/bin/env python3
"""Exercise an opt-in shared GTK terminal on a private Wayland compositor."""
import argparse
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile
import time

from native import Atspi, accessible_text, first_named, wait_for


def exercise(app, process, root, env, wtype, ime_client, pointer_client, spawn, shared_runtime):
    def named(name, state=Atspi.StateType.SHOWING):
        return first_named(process.pid, name, state)

    def screen():
        return named("Shared terminal screen", Atspi.StateType.FOCUSED)

    def key(*args):
        subprocess.run([wtype, "-s", "70", *args], env=env, check=True, timeout=5)

    def command(text):
        key(text)
        key("-k", "Return")

    def clipboard():
        return subprocess.check_output(["wl-paste", "--no-newline"], env=env, timeout=5)

    new_tab = wait_for("Wayland shared tab action", lambda: named("New shared terminal tab"))
    assert new_tab.get_action_iface().do_action(0)
    focused = wait_for("Wayland shared screen focus", screen)
    if shared_runtime:
        assert focused.get_description() == "Shared runtime session", focused.get_description()
    witness = "waylandcopywitness"
    receipt = root / "pty-receipt"
    command(f"printf '\\033[2J\\033[H%s\\033[H' {witness}; stty raw -echo; exec cat > "
            + shlex.quote(str(receipt)))

    def read_witness():
        text = accessible_text(screen())
        return text if re.search(rf"(?m)^{witness} *$", text) else None

    wait_for("AT-SPI terminal text", read_witness)
    wait_for("live shared PTY capture", receipt.exists)
    control = root / "ime-control"
    control.mkdir()
    ime = spawn([ime_client, str(control)], "ime", env)
    wait_for("Wayland input method connected", (control / "connected").exists)
    (control / "start").touch()
    wait_for("compositor delivered input-method preedit", (control / "preedit").exists)
    composition = wait_for("mapped accessible GTK preedit", lambda: named("ni"))
    assert composition.get_description() == "Terminal composition"
    assert receipt.read_bytes() == b"", "composition reached the PTY before commit"
    (control / "commit").touch()
    wait_for("compositor delivered input-method commit", (control / "committed").exists)
    committed = "你".encode()
    wait_for("composed UTF-8 bytes in the live PTY", lambda: receipt.stat().st_size >= len(committed))
    assert receipt.read_bytes() == committed, receipt.read_bytes()
    wait_for("GTK preedit cleared after commit", lambda: named("ni") is None)
    (control / "finish").touch()
    assert ime.wait(timeout=5) == 0

    clipboard_text = b"wayland-paste\n"
    clipboard_file = root / "clipboard-payload"
    clipboard_file.write_bytes(clipboard_text)
    with clipboard_file.open("rb") as data:
        spawn(["wl-copy", "--foreground", "--type", "text/plain;charset=utf-8"], "clipboard", env, data)
    wait_for("external Wayland clipboard ownership", lambda: clipboard() == clipboard_text)
    key("-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl")
    expected = committed + clipboard_text.rstrip(b"\n")
    wait_for("clipboard in the live PTY", lambda: receipt.stat().st_size >= len(expected))
    time.sleep(0.2)
    assert receipt.read_bytes() == expected, receipt.read_bytes()

    trace = (root / f"{app}.log").read_text(errors="replace")
    preedit = re.search(r'zwp_text_input_v3@(\d+)\.preedit_string\("ni", 2, 2\)', trace)
    assert preedit, "missing text-input-v3 preedit"
    context = f"zwp_text_input_v3@{preedit[1]}"
    assert f'{context}.commit_string("你")' in trace
    location = rf"{context}\.set_cursor_rectangle\((\d+), (\d+), (\d+), (\d+)\)"
    rectangles = [tuple(map(int, match)) for match in re.findall(location, trace[:preedit.start()])]
    assert rectangles and all(measure > 0 for measure in rectangles[-1]), rectangles
    if pointer_client:
        # The shell homed the grid cursor over the leftmost character of the
        # witness. Convert from surface geometry to the single Sway output.
        origins = re.findall(r"set_window_geometry\((\d+), (\d+), 1280, 900\)", trace)
        assert origins, "missing Wayland window geometry"
        origin_x, origin_y = map(int, origins[-1])
        x, y, _, height = rectangles[-1]
        pointer = spawn([pointer_client], "pointer", env, subprocess.PIPE)
        pointer_log = root / "pointer.log"
        wait_for("Wayland virtual pointer", lambda: "ready" in pointer_log.read_text())
        click = f"click {x - origin_x + 32} {y - origin_y + height // 2} 1280 900\n"
        pointer.stdin.write(click.encode() * 2)
        pointer.stdin.flush()
        wait_for("real Wayland double-click", lambda: pointer_log.read_text().count("done") >= 2)
        key("-M", "ctrl", "-M", "shift", "-k", "c", "-m", "shift", "-m", "ctrl")
        wait_for("Wayland clipboard contains the selected terminal word", lambda: clipboard() == witness.encode())
        assert receipt.read_bytes() == expected, receipt.read_bytes()
    clipboard_checks = "clipboard paste and pointer copy" if pointer_client else "clipboard paste"
    print(f"{app} native Wayland passed: GTK accessible preedit, cursor rectangle, exact UTF-8 PTY commit and {clipboard_checks}")


def run(args):
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    env = {name: os.environ[name] for name in ("PATH", "DBUS_SESSION_BUS_ADDRESS") if name in os.environ}
    env.update(LC_ALL="C.UTF-8", GDK_BACKEND="wayland", GTK_IM_MODULE="wayland",
               GTK_A11Y="atspi", GSK_RENDERER="cairo", GIO_USE_VFS="local")
    for name in ("XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "HOME"):
        path = root / name
        path.mkdir(mode=0o700)
        env[name] = str(path)
    # The Wayland and Sway Unix-domain socket names must fit inside sun_path.
    runtime = tempfile.TemporaryDirectory(prefix="jterm-gtk-wayland-", dir="/tmp")
    env["XDG_RUNTIME_DIR"] = runtime.name
    compositor_config = root / "sway.conf"
    compositor_config.write_text(
        "xwayland disable\noutput * resolution 1280x900\nseat seat0 fallback true\ndefault_border none\n"
    )
    compositor_env = dict(env, WLR_BACKENDS="headless", WLR_RENDERER="pixman", WLR_LIBINPUT_NO_DEVICES="1")
    if args.compositor_libraries:
        compositor_env["LD_LIBRARY_PATH"] = str(args.compositor_libraries.resolve(strict=True))
    config = root / f"{args.app}.toml"
    config.write_text('shell = "/bin/sh"\nterminal_mode = "vte"\n')
    config.chmod(0o600)
    env[f"{args.app.upper()}_SHELL"] = "/bin/sh"
    if args.shared_runtime:
        env["JTERM_SHARED_RUNTIME"] = "1"
    children = []

    def spawn(argv, name, environment, stdin=subprocess.DEVNULL):
        with (root / f"{name}.log").open("wb") as log:
            child = subprocess.Popen(argv, env=environment, cwd=root, stdin=stdin, stdout=log, stderr=subprocess.STDOUT)
        children.append(child)
        return child

    try:
        compositor = spawn([args.sway, "--unsupported-gpu", "-c", str(compositor_config)], "compositor", compositor_env)

        def display_socket():
            if compositor.poll() is not None:
                raise RuntimeError((root / "compositor.log").read_text())
            return next((p for p in Path(runtime.name).glob("wayland-*") if p.is_socket()), None)

        socket = wait_for("isolated Wayland display", display_socket)
        env["WAYLAND_DISPLAY"] = socket.name
        # Keep the virtual key capability present while the compositor input
        # method operates between commands that start short-lived keyboards.
        spawn([args.wtype, "-s", "600000", "-k", "F24"], "seat", env)
        process = spawn([str(args.binary.resolve(strict=True)), "--config", str(config), "--no-restore", str(root)],
                        args.app, dict(env, WAYLAND_DEBUG="client"))
        Atspi.init()
        pointer = str(args.pointer_client.resolve(strict=True)) if args.pointer_client else None
        exercise(args.app, process, root, env, args.wtype, str(args.ime_client.resolve(strict=True)),
                 pointer, spawn, args.shared_runtime)
    finally:
        for child in reversed(children):
            child.terminate()
        for child in reversed(children):
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=3)
        runtime.cleanup()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", choices=("forge", "anvil"))
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--ime-client", type=Path, required=True)
    parser.add_argument("--pointer-client", type=Path)
    parser.add_argument("--shared-runtime", action="store_true")
    parser.add_argument("--sway", default="sway")
    parser.add_argument("--wtype", default="wtype")
    parser.add_argument("--compositor-libraries", type=Path)
    run(parser.parse_args())


if __name__ == "__main__":
    main()
