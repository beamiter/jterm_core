#!/usr/bin/env python3
"""Exercise a candidate GTK app's opt-in shared terminal in an isolated X11 desktop."""
import argparse
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile
import time

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi, GLib


def wait_for(description, check):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (GLib.Error, subprocess.CalledProcessError):
            pass
        time.sleep(0.05)
    raise RuntimeError(f"Timed out waiting for {description}")


def descendants(node):
    if node is not None:
        yield node
        for index in range(node.get_child_count()):
            yield from descendants(node.get_child_at_index(index))


def first_named(pid, name, state=Atspi.StateType.SHOWING):
    desktop = Atspi.get_desktop(0)
    for index in range(desktop.get_child_count()):
        application = desktop.get_child_at_index(index)
        if application is not None and application.get_process_id() == pid:
            return next((node for node in descendants(application)
                         if node.get_name() == name and node.get_state_set().contains(state)), None)
    return None


def accessible_text(node):
    if node is None:
        return ""
    text = node.get_text_iface()
    return Atspi.Text.get_text(text, 0, Atspi.Text.get_character_count(text))


def run(app, binary, root, underline_colors, osc8_links, shared_runtime):
    config = root / f"{app}.toml"
    config.write_text('shell = "/bin/sh"\nterminal_mode = "vte"\n')
    config.chmod(0o600)
    environment = os.environ.copy()
    for variable in (
        "HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR",
    ):
        location = root / variable
        location.mkdir(mode=0o700)
        environment[variable] = str(location)
    environment.update(GDK_BACKEND="x11", GSK_RENDERER="cairo", GTK_A11Y="atspi")
    environment[f"{app.upper()}_SHELL"] = "/bin/sh"
    if shared_runtime:
        environment["JTERM_SHARED_RUNTIME"] = "1"
    if osc8_links:
        install_link_handler(root, environment)
    with (root / f"{app}.log").open("w+") as log:
        process = subprocess.Popen(
            [binary, "--config", str(config), "--no-restore", str(root)],
            env=environment, stdout=log, stderr=log,
        )
        try:
            exercise(app, process.pid, root, underline_colors, osc8_links, shared_runtime)
        except Exception:
            log.seek(0)
            print(log.read().strip())
            raise
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def install_link_handler(root, environment):
    handler = root / "open-link"
    receipt = root / "links-opened"
    handler.write_text(f"#!/bin/sh\nprintf '%s\\n' \"$1\" >> {shlex.quote(str(receipt))}\n")
    handler.chmod(0o700)
    applications = Path(environment["XDG_DATA_HOME"]) / "applications"
    applications.mkdir()
    (applications / "terminal-test-link.desktop").write_text(
        "[Desktop Entry]\nType=Application\nName=Terminal Test Link Handler\n"
        f"Exec={handler} %u\nMimeType=x-scheme-handler/https;\nNoDisplay=true\n"
    )
    (Path(environment["XDG_CONFIG_HOME"]) / "mimeapps.list").write_text(
        "[Default Applications]\nx-scheme-handler/https=terminal-test-link.desktop;\n"
        "[Added Associations]\nx-scheme-handler/https=terminal-test-link.desktop;\n"
    )
    environment["GIO_USE_PORTALS"] = "0"
    default = subprocess.check_output(
        ["gio", "mime", "x-scheme-handler/https"], env=environment, text=True, timeout=5,
    ).splitlines()[0]
    assert default.endswith("terminal-test-link.desktop"), default


def exercise(app, pid, root, underline_colors, osc8_links, shared_runtime):
    def command(*args):
        subprocess.run(["xdotool", *args], check=True, timeout=5)

    def type_command(text):
        command("type", "--clearmodifiers", "--delay", "5", text)
        command("key", "Return")

    def open_tab():
        button = wait_for("shared terminal action", lambda: first_named(pid, "New shared terminal tab"))
        previous = focused_screen()
        assert button.get_action_iface().do_action(0)

        def new_screen():
            screen = focused_screen()
            return screen if screen != previous else None

        screen = wait_for("shared screen focus", new_screen)
        if shared_runtime:
            assert screen.get_description() == "Shared runtime session", screen.get_description()

    def focused_screen():
        return first_named(pid, "Shared terminal screen", Atspi.StateType.FOCUSED)

    def read_screen():
        return accessible_text(focused_screen())

    def output(pattern):
        return wait_for(f"shell output {pattern}", lambda: re.search(pattern, read_screen()))

    def first_history_row():
        match = re.search(r"(?m)^scroll-row-(\d+)", read_screen())
        return int(match[1]) if match else None

    def history_index(description, matches):
        def check():
            row = first_history_row()
            return row if row is not None and matches(row) else None

        return wait_for(description, check)

    def screen_extents():
        screen = focused_screen()
        return screen.get_component_iface().get_extents(Atspi.CoordType.WINDOW) if screen else None

    window = wait_for(f"{app} X11 window", lambda: subprocess.check_output(
        ["xdotool", "search", "--onlyvisible", "--pid", str(pid)], text=True,
        stderr=subprocess.DEVNULL, timeout=5,
    ).split()[0])
    command("windowfocus", "--sync", window)
    open_tab()
    type_command(r'''printf 'first-shell [%s]\n' "$TERM"''')
    output(r"(?m)^first-shell \[dumb\]")
    if shared_runtime:
        type_command(r'''printf 'profile=[%s]\n' "${JTERM_SHARED_RUNTIME-unset}"''')
        output(r"(?m)^profile=\[unset\]")
        receipt = root / "runtime-query-bytes"
        type_command(
            r"stty raw -echo; printf '\r\nruntime-dsr-ready\r\n\033[5n'; head -c 5 > "
            + shlex.quote(str(receipt))
            + r"; stty sane; printf '\r\nruntime-dsr-done\r\n'"
        )
        output(r"(?m)^runtime-dsr-ready *$")
        command("key", "x")
        output(r"(?m)^runtime-dsr-done *$")
        assert receipt.read_bytes() == b"\x1b[0nx", receipt.read_bytes()
        print(f"{app} native common runtime verified exact PTY reply/input order")
    clipboard = r'''printf 'clipboard [%s]\n' "$TERM"''' + "\n"
    subprocess.run(["xclip", "-selection", "clipboard", "-in"],
                   input=clipboard, text=True, check=True, timeout=5)
    command("key", "ctrl+shift+v")
    wait_for("clipboard draft on screen", lambda: "clipboard [%s]" in read_screen())
    assert "clipboard [dumb]" not in read_screen(), "clipboard paste submitted a command"
    command("key", "Return")
    output(r"(?m)^clipboard \[dumb\]")
    type_command(r'''printf 'grid=[%s]\n' "$(stty size)"''')
    before = output(r"(?m)^grid=\[(\d+ \d+)\]")[1]
    command("windowsize", window, "800", "520")
    time.sleep(0.3)
    type_command(r'''printf 'resized-grid=[%s]\n' "$(stty size)"''')
    after = output(r"(?m)^resized-grid=\[(\d+ \d+)\]")[1]
    assert before != after, (before, after)

    type_command(r'''i=1; while [ "$i" -le 120 ]; do printf 'scroll-row-%03d\n' "$i"; i=$((i+1)); done''')
    output(r"(?m)^scroll-row-120")
    live = first_history_row()
    assert live > 1, live
    command("key", "shift+Home")
    oldest = history_index("oldest retained page", lambda row: row == 1)
    command("key", "shift+Page_Down")
    page = history_index("next scrollback page", lambda row: oldest < row < live)
    command("key", "shift+Page_Up")
    history_index("previous scrollback page", lambda row: row == oldest)
    bounds = wait_for("terminal screen coordinates", screen_extents)
    command("key", "shift+Page_Down")
    history_index("wheel starting page", lambda row: row == page)
    command("mousemove", "--window", window, str(bounds.x + bounds.width // 2),
            str(bounds.y + bounds.height // 2), "click", "5")
    history_index("wheel down moved scrollback", lambda row: page < row < live)
    command("click", "4")
    history_index("wheel up returned to original scrollback", lambda row: row == page)
    command("key", "shift+End")
    output(r"(?m)^scroll-row-120")

    visible_marker = root / "visible-ready"
    hidden_marker = root / "hidden-ready"
    release_hidden = root / "emit-hidden"
    type_command(
        r'''(sleep 1; printf 'while-reading [%s]\n' "$TERM"; : > '''
        + shlex.quote(str(visible_marker))
        + r'''; n=0; while [ "$n" -lt 200 ]; do if [ -e '''
        + shlex.quote(str(release_hidden))
        # Exceed the PTY buffer without displacing the reading page. Completion
        # then requires the hidden tab's reader to keep consuming output.
        + r''' ]; then awk 'BEGIN {for (i=0; i<4096; i++) printf "hidden-transport-probe\r"}'; '''
        + r'''printf '\r\033[Kwhile-hidden [%s]\n' "$TERM"; : > '''
        + shlex.quote(str(hidden_marker))
        + r'''; break; fi; n=$((n+1)); sleep 0.1; done) &'''
    )
    command("key", "shift+Home")
    history_index("pinned reading page", lambda row: row == oldest)
    wait_for("foreground tab output while reading", visible_marker.exists)
    assert first_history_row() == oldest
    assert "while-reading [dumb]" not in read_screen()
    pinned_rows = len(read_screen().splitlines())
    command("windowsize", window, "1000", "680")
    try:
        wait_for("resized pinned viewport", lambda: len(read_screen().splitlines()) > pinned_rows)
    except RuntimeError as error:
        geometry = subprocess.check_output(["xdotool", "getwindowgeometry", window], text=True, timeout=5)
        raise RuntimeError(f"pinned viewport rows {pinned_rows} to {len(read_screen().splitlines())}; {geometry}") from error
    assert first_history_row() == oldest

    open_tab()
    type_command(r'''printf 'second-shell [%s]\n' "$TERM"''')
    output(r"(?m)^second-shell \[dumb\]")
    assert "first-shell [dumb]" not in read_screen()
    release_hidden.touch()
    wait_for("command completion in the hidden shell", hidden_marker.exists)
    type_command("exit 7")
    wait_for("reported shell exit", lambda: first_named(pid, "Shell exited (7)"))
    command("key", "ctrl+shift+w")
    history_index("restored first tab's reading page", lambda row: row == oldest)
    assert "while-hidden [dumb]" not in read_screen()
    command("key", "shift+End")
    output(r"(?m)^while-hidden \[dumb\]")
    assert "second-shell [dumb]" not in read_screen()
    command("key", "shift+Home")
    history_index("first tab scrollback", lambda row: row == oldest)
    type_command(r'''printf 'returned-live [%s]\n' "$TERM"''')
    output(r"(?m)^returned-live \[dumb\]")
    print(f"{app} X11 shared terminal passed: PTY {before} to {after}, clipboard, native scrollback, pinned resize, independent tabs, hidden output and exit")
    if underline_colors:
        check_underline_colors(app, window, type_command, command, output, screen_extents)
    if osc8_links:
        check_osc8_links(app, pid, root, window, type_command, command, output, screen_extents)


def sample_pixels(window, bounds, width, height):
    pixels = subprocess.check_output([
        "import", "-window", window, "-crop", f"{width}x{height}+{bounds.x}+{bounds.y}",
        "-depth", "8", "rgb:-",
    ], timeout=5)
    assert len(pixels) == width * height * 3, len(pixels)
    return list(zip(pixels[0::3], pixels[1::3], pixels[2::3]))


def check_underline_colors(app, window, type_command, command, output, screen_extents):
    rgb = (18, 227, 113)
    indexed = (255, 95, 0)  # xterm color 202
    witness = "underlinecolorwitness"
    type_command(
        rf"stty -echo; printf '\033[2J\033[3J\033[H\033[?25l\033[4;58;2;18;227;113m{witness}\033[0m'; "
        rf"IFS= read -r phase; printf '\033[H\033[4;58;5;202m{witness}\033[0m'; "
        r"IFS= read -r phase; printf '\033[2;1H'; i=1; "
        r'''while [ "$i" -le 120 ]; do printf 'underline-scroll-%03d\n' "$i"; i=$((i+1)); done; '''
        r"stty echo; printf '\033[?25h'"
    )
    output(rf"(?m)^{witness}")
    bounds = wait_for("underline capture viewport", screen_extents)
    width = min(bounds.width, 500)
    height = min(bounds.height, 100)

    def sample():
        return sample_pixels(window, bounds, width, height)

    def contains(color):
        pixels = sample()
        return pixels if pixels.count(color) >= 20 else None

    first = wait_for("RGB-only underline pixels", lambda: contains(rgb))
    command("key", "Return")
    second = wait_for("indexed underline pixels", lambda: contains(indexed))
    colors_changed = [(i, a, b) for i, (a, b) in enumerate(zip(first, second)) if a != b]
    replaced = [i for i, a, b in colors_changed if a == rgb and b == indexed]
    assert len(replaced) >= 20, len(replaced)
    underline_lines = {i // width for i in replaced}
    assert {i // width for i, _, _ in colors_changed}.issubset(underline_lines), underline_lines

    command("key", "Return")
    output(r"(?m)^underline-scroll-120")
    assert indexed not in sample(), "styled row was not scrolled out of the live buffer"
    command("key", "shift+Home")
    output(rf"(?m)^{witness}")
    archived = wait_for("indexed underline in history", lambda: contains(indexed))
    assert {i for i, pixel in enumerate(archived) if pixel == indexed} == {
        i for i, pixel in enumerate(second) if pixel == indexed
    }
    print(f"{app} X11 underline colors passed: {len(replaced)} RGB to indexed pixels only on the underline, with exact history retention")


def check_osc8_links(app, pid, root, window, type_command, command, output, screen_extents):
    live = "https://example.test/shared-live"
    receipt = root / "links-opened"
    type_command(
        rf"stty -echo; printf '\033[2J\033[3J\033[H\033]8;;{live}\033\\\033[4;58;2;18;227;113mclicklivewitness\033[0m\033]8;;\033\\'; "
        r"printf '\033[2;1H\033]8;;file:///nonexistent\033\\\033[4;58;2;227;18;113mcliprejectedwitness\033[0m\033]8;;\033\\'; "
        r"IFS= read -r phase; printf '\033[3;1H'; i=1; "
        r'''while [ "$i" -le 120 ]; do printf 'link-scroll-%03d\n' "$i"; i=$((i+1)); done; stty echo'''
    )
    output(r"(?m)^cliprejectedwitness")
    bounds = wait_for("OSC 8 viewport coordinates", screen_extents)
    width = min(bounds.width, 500)
    height = min(bounds.height, 100)
    approved = (18, 227, 113)
    rejected = (227, 18, 113)

    def click(color, *, modified=True):
        def probe():
            pixels = sample_pixels(window, bounds, width, height)
            return [i for i, pixel in enumerate(pixels) if pixel == color]

        pixels = wait_for("mapped OSC 8 witness pixels", probe)
        center = pixels[len(pixels) // 2]
        command("mousemove", "--window", window, str(bounds.x + center % width),
                str(bounds.y + center // width - 4))
        if modified:
            command("keydown", "ctrl", "click", "1", "keyup", "ctrl")
        else:
            command("click", "1")

    def targets():
        return receipt.read_text().splitlines() if receipt.exists() else []

    click(approved, modified=False)
    click(rejected)
    time.sleep(0.2)
    assert targets() == [], targets()
    click(approved)
    try:
        wait_for("exact OSC 8 link receipt", lambda: targets() == [live])
    except RuntimeError as error:
        failed = first_named(pid, "Could not open link") is not None
        raise RuntimeError(f"OSC 8 received {targets()}, application opener dialog {failed}") from error

    command("key", "Return")
    output(r"(?m)^link-scroll-120")
    command("key", "shift+Home")
    output(r"(?m)^clicklivewitness")
    # Reset the click count after the earlier live click before testing history.
    time.sleep(0.3)
    click(approved)
    wait_for("exact retained OSC 8 link receipt", lambda: targets() == [live, live])
    print(f"{app} X11 OSC 8 passed: explicit live and history clicks sent exact HTTPS targets to an isolated local opener")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", choices=("forge", "anvil"))
    parser.add_argument("binary", type=Path)
    parser.add_argument("--underline-colors", action="store_true", help="compare underline pixels with ImageMagick import")
    parser.add_argument("--osc8-links", action="store_true", help="verify OSC 8 clicks through an isolated local URI handler")
    parser.add_argument("--shared-runtime", action="store_true", help="use and verify the common PTY and terminal owner")
    args = parser.parse_args()
    Atspi.init()
    with tempfile.TemporaryDirectory(prefix=f"{args.app}-shared-terminal-") as directory:
        run(args.app, args.binary.resolve(), Path(directory), args.underline_colors, args.osc8_links, args.shared_runtime)


if __name__ == "__main__":
    main()
