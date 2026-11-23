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

from native import Atspi, accessible_text, first_named, start_accessibility, wait_for

OUTPUT_WIDTH = 1280
OUTPUT_HEIGHT = 900


def exercise(args, process, root, env, ime, control, pointer_client, spawn):
    app = args.app
    def named(name, state=Atspi.StateType.SHOWING):
        return first_named(process.pid, name, state)

    def screen():
        return named("Shared terminal screen", Atspi.StateType.FOCUSED)

    def key(*sequence):
        subprocess.run([args.wtype, "-s", "70", *sequence], env=env, check=True, timeout=5)

    def command(text):
        key(text)
        key("-k", "Return")

    def clipboard():
        return subprocess.check_output(["wl-paste", "--no-newline"], env=env, timeout=5)

    if not args.ordinary_pane:
        new_tab = wait_for("Wayland shared tab action", lambda: named("New shared terminal tab"))
        assert new_tab.get_action_iface().do_action(0)
    focused = wait_for("Wayland shared screen focus", screen)
    if args.shared_runtime or args.ordinary_pane:
        assert focused.get_description() == "Shared runtime session", focused.get_description()
    witness = "waylandcopywitness"
    receipt = root / "pty-receipt"
    negotiate = r"printf '\033[>31u\033[?u'; " if args.keyboard_disambiguation else ""
    command(f"printf '\\033[2J\\033[H%s\\033[H' {witness}; stty raw -echo; "
            + negotiate + "exec cat > " + shlex.quote(str(receipt)))

    def read_witness():
        text = accessible_text(screen())
        return text if re.search(rf"(?m)^{witness} *$", text) else None

    wait_for("AT-SPI terminal text", read_witness)
    wait_for("live shared PTY capture", receipt.exists)
    keyboard_reply = b"\x1b[?1u" if args.keyboard_disambiguation else b""
    wait_for("keyboard negotiation reply", lambda: receipt.read_bytes() == keyboard_reply)
    (control / "start").touch()
    wait_for("compositor delivered input-method preedit", (control / "preedit").exists)
    composition = wait_for("mapped accessible GTK preedit", lambda: named("ni"))
    assert composition.get_description() == "Terminal composition"
    assert receipt.read_bytes() == keyboard_reply, "composition reached the PTY before commit"
    (control / "commit").touch()
    wait_for("compositor delivered input-method commit", (control / "committed").exists)
    committed = keyboard_reply + "你".encode()
    wait_for("composed UTF-8 bytes in the live PTY", lambda: receipt.stat().st_size >= len(committed))
    assert receipt.read_bytes() == committed, receipt.read_bytes()
    wait_for("GTK preedit cleared after commit", lambda: named("ni") is None)
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
    logical_width, logical_height = OUTPUT_WIDTH // args.scale, OUTPUT_HEIGHT // args.scale
    origins = re.findall(rf"set_window_geometry\((\d+), (\d+), {logical_width}, {logical_height}\)", trace)
    assert origins, "missing expected Wayland window geometry"
    if args.scale > 1:
        assert re.search(rf"wl_output[@#]\d+\.scale\({args.scale}\)", trace), "missing HiDPI output scale"
    # Newer libwayland loggers use # in place of @ for the same object ID.
    preedit = re.search(r'(zwp_text_input_v3[@#]\d+)\.preedit_string\("ni", 2, 2\)', trace)
    assert preedit, "missing text-input-v3 preedit"
    context = preedit[1]
    commit = trace.index(f'{context}.commit_string("你")', preedit.end())
    location = rf"{context}\.set_cursor_rectangle\((\d+), (\d+), (\d+), (\d+)\)"
    rectangles = [tuple(map(int, match)) for match in re.findall(location, trace[preedit.end():commit])]
    assert rectangles and all(measure > 0 for measure in rectangles[-1]), rectangles
    if pointer_client:
        # The shell homed the grid cursor over the leftmost character of the
        # witness. Convert from surface geometry to the single Sway output.
        origin_x, origin_y = map(int, origins[-1])
        x, y, _, height = rectangles[-1]
        pointer = spawn([pointer_client], "pointer", env, subprocess.PIPE)
        pointer_log = root / "pointer.log"
        wait_for("Wayland virtual pointer", lambda: "ready" in pointer_log.read_text())
        click = f"click {x - origin_x + 32} {y - origin_y + height // 2} {logical_width} {logical_height}\n"
        pointer.stdin.write(click.encode() * 2)
        pointer.stdin.flush()
        wait_for("real Wayland double-click", lambda: pointer_log.read_text().count("done") >= 2)
        key("-M", "ctrl", "-M", "shift", "-k", "c", "-m", "shift", "-m", "ctrl")
        wait_for("Wayland clipboard contains the selected terminal word", lambda: clipboard() == witness.encode())
        assert receipt.read_bytes() == expected, receipt.read_bytes()
    clipboard_checks = "clipboard paste and pointer copy" if pointer_client else "clipboard paste"
    print(f"{app} native Wayland passed at scale {args.scale}: GTK accessible preedit, cursor rectangle, exact UTF-8 PTY commit and {clipboard_checks}")

    if args.ordinary_pane:
        def new_input_method(name):
            directory = root / name
            directory.mkdir()
            method = spawn([str(args.ime_client), str(directory)], name, env)
            wait_for("replacement input method became available", (directory / "connected").exists)
            return directory, method

        old_screen = screen()
        pending, canceled = new_input_method("ime-pending")
        (pending / "start").touch()
        wait_for("uncommitted composition in the first pane", lambda: named("ni"))
        assert receipt.read_bytes() == expected
        key("-M", "ctrl", "-M", "shift", "-k", "t", "-m", "shift", "-m", "ctrl")
        second = wait_for("focus in a different shared terminal", lambda: (
            node if (node := screen()) is not None and node != old_screen else None))
        assert second.get_description() == "Shared runtime session"
        wait_for("old composition cleared on focus transfer", lambda: named("ni") is None)
        canceled.terminate()
        canceled.wait(timeout=5)

        second_receipt = root / "second-pty-receipt"
        command("stty raw -echo; " + negotiate + "exec cat > " + shlex.quote(str(second_receipt)))
        wait_for("second PTY has no interrupted composition bytes", lambda: (
            second_receipt.is_file() and second_receipt.read_bytes() == keyboard_reply))
        next_control, next_ime = new_input_method("ime-second")
        (next_control / "start").touch()
        wait_for("fresh preedit in the second pane", lambda: named("ni"))
        assert second_receipt.read_bytes() == keyboard_reply
        (next_control / "commit").touch()
        wait_for("replacement compositor delivered input-method commit", (next_control / "committed").exists)
        assert next_ime.wait(timeout=5) == 0
        wait_for("fresh commit in the second PTY", lambda: second_receipt.read_bytes() == committed)
        assert receipt.read_bytes() == expected
        print(f"{app} native Wayland focus transfer passed: pending preedit cleared and one fresh commit reached the new pane")


def exercise_vim9(args, process, root, env, ime, control, document):
    def key(*sequence):
        subprocess.run([args.wtype, "-s", "70", *sequence], env=env, check=True, timeout=5)

    def editor():
        return first_named(process.pid, "Vim9 editor", Atspi.StateType.FOCUSED)

    screen = wait_for("focused Vim9 editor", editor)
    assert screen.get_description() == "Shared runtime session"
    display = "GUI · RGB / Pango" if args.vim9 == "native" else "Vim · compatibility display"
    wait_for(display, lambda: first_named(process.pid, display))
    wait_for("real Vim9 buffer content", lambda: "ime-original" in accessible_text(editor()))
    key("-k", "i")
    wait_for("Vim is in insert mode", lambda: "INSERT" in accessible_text(editor()))
    (control / "start").touch()
    wait_for("Vim9 compositor preedit", (control / "preedit").exists)
    assert "niime-original" not in accessible_text(editor())
    (control / "commit").touch()
    wait_for("Vim9 compositor commit", (control / "committed").exists)
    wait_for("exact committed text in the Vim buffer", lambda: "你ime-original" in accessible_text(editor()))
    assert ime.wait(timeout=5) == 0

    trace = (root / f"{args.app}.log").read_text(errors="replace")
    preedit = re.search(r'(zwp_text_input_v3[@#]\d+)\.preedit_string\("ni", 2, 2\)', trace)
    assert preedit
    context = preedit[1]
    commit = trace.index(f'{context}.commit_string("你")', preedit.end())
    rectangles = re.findall(rf"{context}\.set_cursor_rectangle\((\d+), (\d+), (\d+), (\d+)\)", trace[:commit])
    assert rectangles and int(rectangles[-1][2]) > 0 and int(rectangles[-1][3]) > 0, rectangles
    key("-k", "Escape")
    key(":wq")
    key("-k", "Return")
    assert process.wait(timeout=10) == 0
    assert document.read_text(encoding="utf-8") == "你ime-original\n"
    print(f"{args.app} native Wayland Vim9 {args.vim9} passed: preedit, one UTF-8 commit and real file save")


def run(args):
    root = args.output.resolve()
    root.mkdir(mode=0o700, parents=True, exist_ok=False)
    start_accessibility()
    env = {name: os.environ[name] for name in ("PATH", "DBUS_SESSION_BUS_ADDRESS", "AT_SPI_BUS_ADDRESS") if name in os.environ}
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
        f"xwayland disable\noutput * resolution {OUTPUT_WIDTH}x{OUTPUT_HEIGHT} scale {args.scale}\n"
        "seat seat0 fallback true\ndefault_border none\n"
    )
    compositor_env = dict(env, WLR_BACKENDS="headless", WLR_RENDERER="pixman", WLR_LIBINPUT_NO_DEVICES="1")
    if args.compositor_libraries:
        compositor_env["LD_LIBRARY_PATH"] = str(args.compositor_libraries.resolve(strict=True))
    config = root / f"{args.app}.toml"
    contents = 'shell = "/bin/sh"\nterminal_mode = "vte"\n'
    document = root / "vim-ime.txt"
    if args.vim9:
        contents += f'[vim9]\nnative_buffer = {str(args.vim9 == "native").lower()}\nstatus_bar = true\n'
        document.write_text("ime-original\n", encoding="utf-8")
    config.write_text(contents)
    config.chmod(0o600)
    env[f"{args.app.upper()}_SHELL"] = "/bin/sh"
    if args.shared_runtime and not args.vim9:
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
        control = root / "ime-control"
        control.mkdir()
        ime = spawn([str(args.ime_client.resolve(strict=True)), str(control)], "ime", env)
        wait_for("Wayland input method connected", (control / "connected").exists)
        target = ["--ide", str(document)] if args.vim9 else ["--no-restore", str(root)]
        process = spawn([str(args.binary.resolve(strict=True)), "--config", str(config), *target],
                        args.app, dict(env, WAYLAND_DEBUG="client"))
        if args.vim9:
            exercise_vim9(args, process, root, env, ime, control, document)
        else:
            pointer = str(args.pointer_client.resolve(strict=True)) if args.pointer_client else None
            exercise(args, process, root, env, ime, control, pointer, spawn)
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
    parser.add_argument("--ordinary-pane", action="store_true", help="use the normal startup terminal")
    parser.add_argument("--keyboard-disambiguation", action="store_true",
                        help="negotiate Kitty disambiguation before IME composition")
    parser.add_argument("--vim9", choices=("native", "compatibility"), help="compose and save inside real Vim9")
    parser.add_argument("--sway", default="sway")
    parser.add_argument("--wtype", default="wtype")
    parser.add_argument("--compositor-libraries", type=Path)
    parser.add_argument("--scale", type=int, choices=(1, 2), default=1, help="private Wayland output scale")
    run(parser.parse_args())


if __name__ == "__main__":
    main()
