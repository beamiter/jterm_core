#!/usr/bin/env python3
"""Exercise a candidate GTK app's opt-in shared terminal in an isolated X11 desktop."""
import argparse
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import tempfile
import time

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi, Gio, GLib


def start_accessibility():
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    result = bus.call_sync("org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress",
                           None, GLib.VariantType.new("(s)"), Gio.DBusCallFlags.NONE, 5000, None)
    os.environ["AT_SPI_BUS_ADDRESS"] = result.unpack()[0]
    Atspi.init()


def wait_for(description, check):
    deadline = time.monotonic() + 10
    context = GLib.MainContext.default()
    while time.monotonic() < deadline:
        try:
            for _ in range(16):
                if not context.iteration(False):
                    break
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


def application_nodes(pid):
    desktop = Atspi.get_desktop(0)
    for index in range(desktop.get_child_count()):
        application = desktop.get_child_at_index(index)
        if application is not None and application.get_process_id() == pid:
            dynamic = Atspi.Cache.STATES | Atspi.Cache.CHILDREN | Atspi.Cache.NAME
            application.set_cache_mask(Atspi.Cache(Atspi.Cache.DEFAULT & ~dynamic))
            yield from descendants(application)


def first_named(pid, name, state=Atspi.StateType.SHOWING):
    return next((node for node in application_nodes(pid)
                 if node.get_name() == name and node.get_state_set().contains(state)), None)


def accessible_text(node):
    if node is None:
        return ""
    text = node.get_text_iface()
    return Atspi.Text.get_text(text, 0, Atspi.Text.get_character_count(text))


def run(args, root):
    app = args.app
    binary = args.binary.resolve(strict=True)
    shell = f"/bin/{args.shell}"
    config = root / f"{app}.toml"
    config.write_text(f'shell = "{shell}"\nterminal_mode = "vte"\n'
                      + ('scrollback = 1500\n' if args.scrollback_policy else ""))
    config.chmod(0o600)
    environment = os.environ.copy()
    for variable in (
        "HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR",
    ):
        location = root / variable
        location.mkdir(mode=0o700)
        environment[variable] = str(location)
    environment.update(GDK_BACKEND="x11", GSK_RENDERER="cairo", GTK_A11Y="atspi")
    environment[f"{app.upper()}_SHELL"] = shell
    if args.shell == "bash":
        (Path(environment["HOME"]) / ".bashrc").write_text("printf 'configured-bash-startup\\n'\n")
    if args.shared_runtime or args.ordinary_panes:
        environment["JTERM_SHARED_RUNTIME"] = "1"
    if args.osc8_links:
        install_link_handler(root, environment)
    with (root / f"{app}.log").open("w+") as log:
        process = subprocess.Popen(
            [binary, "--config", str(config), "--no-restore", str(root)],
            env=environment, stdout=log, stderr=log,
        )
        try:
            if args.ordinary_panes:
                exercise_ordinary(args, process.pid, root)
            else:
                exercise(app, process.pid, root, args.underline_colors, args.osc8_links, args.shared_runtime)
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


def exercise_ordinary(args, pid, root):
    app = args.app
    def key(*combos):
        subprocess.run(["xdotool", "key", "--clearmodifiers", *combos], check=True, timeout=5)

    def send(text):
        subprocess.run(["xdotool", "type", "--clearmodifiers", "--delay", "5", text], check=True, timeout=5)
        key("Return")

    def screen():
        return first_named(pid, "Shared terminal screen", Atspi.StateType.FOCUSED)

    def wait_text(line):
        return wait_for(line, lambda: re.search(r"(?m)^" + re.escape(line) + r" *$", accessible_text(screen())))

    def screens():
        desk = Atspi.get_desktop(0)
        return [node for instance in (desk.get_child_at_index(i) for i in range(desk.get_child_count()))
                if instance is not None and instance.get_process_id() == pid for node in descendants(instance)
                if node.get_name() == "Shared terminal screen" and node.get_state_set().contains(Atspi.StateType.SHOWING)]

    window = wait_for("normal terminal window", lambda: subprocess.check_output(
        ["xdotool", "search", "--onlyvisible", "--pid", str(pid)], text=True, stderr=subprocess.DEVNULL,
        timeout=5).split()[0])
    subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True, timeout=5)
    assert wait_for("ordinary startup terminal", screen).get_description() == "Shared runtime session"
    if args.shell == "bash":
        wait_text("configured-bash-startup")
    project = root / "other directory"
    project.mkdir()
    send(f"cd {shlex.quote(str(project))}; " + r"printf 'original-owned\n'")
    wait_text("original-owned")
    title = "Shared pane title witness"
    send(rf"printf '\033]2;{title}\007'")
    wait_for("the tab follows its shell's OSC title", lambda: first_named(pid, title))
    send(r"printf '\033]2;\007'")
    wait_for("the shell returned its tab title", lambda: first_named(pid, title) is None)
    key("ctrl+shift+t")
    wait_for("new ordinary tab", lambda: screen() and "original-owned" not in accessible_text(screen()))
    send(r"""printf 'new-tab-dir=[%s]\n' "$PWD" """)
    wait_text(f"new-tab-dir=[{project}]")

    key("ctrl+shift+e")
    wait_for("two ordinary split screens", lambda: len(screens()) == 2)
    wait_for("new split owns terminal focus", lambda: (view := screen()) is not None
             and "new-tab-dir=" not in accessible_text(view))
    # Compare the full working directory inside the shell. A split may be too
    # narrow to expose that path on one physical accessibility row.
    send(f"test \"$PWD\" = {shlex.quote(str(project))} && printf 'new-split-dir-ok\\n'")
    wait_text("new-split-dir-ok")
    assert [bool(re.search(r"(?m)^new-split-dir-ok *$", accessible_text(view))) for view in screens()].count(True) == 1
    assert all(view.get_description() == "Shared runtime session" for view in screens())
    send("exit 7")
    wait_for("real process exit collapsed only its split", lambda: len(screens()) == 1)
    wait_text(f"new-tab-dir=[{project}]")
    send("exit")
    wait_text("original-owned")
    print(f"{app} X11 ordinary shared shell passed: startup, new tab, directory inheritance, split and child exit")
    if args.tui:
        check_tui(app, root, window, send, key, screen, wait_text)
    if args.keyboard_disambiguation:
        check_keyboard_disambiguation(app, root, send, key, wait_text)
    if args.streamed_paste:
        check_streamed_paste(app, root, send, key, wait_text)
    if args.scrollback_policy:
        check_scrollback_policy(app, send, key, screen, wait_text)
    if args.resize_history:
        check_resize_history(app, window, send, key, screen, wait_text)
        check_history_reflow(app, root, window, send, key, screen, wait_text)
    if args.history_search:
        check_history_search(app, pid, root, window, send, key, screen, wait_text)
    if args.hidden_output_seconds:
        check_hidden_output(app, pid, root, args.hidden_output_seconds, send, key, screen, wait_text)


def check_history_search(app, pid, root, window, send, key, screen, wait_text):
    fixture = root / "history-search.py"
    append = root / "append-search-output"
    query = "AcrossWrapXYZ"
    fixture.write_text(f'''import os
from pathlib import Path
import time
columns = os.get_terminal_size().columns
for age in ("older", "newer"):
    print("x" * (columns - 3) + {query!r} + " " + age + " 中")
    for i in range(112):
        print(f"history filler {{age}} {{i}}")
print("history-search-ready", flush=True)
deadline = time.monotonic() + 25
while not Path({str(append)!r}).exists() and time.monotonic() < deadline:
    time.sleep(0.025)
print({query!r} + " appended")
for i in range(112):
    print(f"archive append {{i}}")
print("history-search-appended", flush=True)
''')
    send(r"printf '\033[3J'")
    send(f"python3 -B {shlex.quote(str(fixture))}")
    wait_text("history-search-ready")

    def open_find():
        key("ctrl+shift+f")
        dialog = wait_for("native history search window", lambda: subprocess.check_output(
            ["xdotool", "search", "--onlyvisible", "--all", "--pid", str(pid), "--name", "^Find terminal history$"],
            text=True, timeout=5, stderr=subprocess.DEVNULL).split()[0])
        subprocess.run(["xdotool", "windowfocus", "--sync", dialog], check=True, timeout=5)
        wait_for("history search entry focused", lambda: first_named(pid, "Find exact text", Atspi.StateType.FOCUSED))
        send(query)
        return dialog

    def match(age):
        result = first_named(pid, "Terminal history match")
        if result is None or f"{query} {age} 中" not in accessible_text(result):
            return False
        text = result.get_text_iface()
        if Atspi.Text.get_n_selections(text) != 1:
            return False
        selected = Atspi.Text.get_selection(text, 0)
        return Atspi.Text.get_text(text, selected.start_offset, selected.end_offset) == query

    def close_find():
        key("Escape")
        wait_for("history search closed", lambda: first_named(pid, "Terminal history match") is None)
        subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True, timeout=5)
        wait_for("returned to the shell", screen)

    dialog = open_find()
    wait_for("newest match joining a soft wrap", lambda: match("newer"))
    key("ctrl+c")
    wait_for("exact native history match clipboard", lambda: subprocess.check_output(
        ["xclip", "-selection", "clipboard", "-o"], stderr=subprocess.DEVNULL, timeout=5) == query.encode())
    append.touch()
    wait_for("PTY continues to archive with search open", lambda: "history-search-appended" in accessible_text(
        first_named(pid, "Shared terminal screen")))
    older = wait_for("older match navigation", lambda: first_named(pid, "Older match"))
    extent = Atspi.Component.get_extents(older.get_component_iface(), Atspi.CoordType.WINDOW)
    subprocess.run(["xdotool", "mousemove", "--window", dialog, str(extent.x + extent.width // 2),
                    str(extent.y + extent.height // 2), "click", "1"], check=True, timeout=5)
    try:
        wait_for("older match in another archive page", lambda: match("older"))
        wait_for("older match is available in the X11 primary selection", lambda: subprocess.check_output(
            ["xclip", "-selection", "primary", "-o"], stderr=subprocess.DEVNULL, timeout=5) == query.encode())
    except RuntimeError:
        labels = [node.get_name() for node in application_nodes(pid) if node.get_role() == Atspi.Role.LABEL]
        result = first_named(pid, "Terminal history match")
        snippets = re.findall(re.escape(query) + r"[^\n]{0,24}", accessible_text(result))
        text = result.get_text_iface() if result else None
        selected = Atspi.Text.get_selection(text, 0) if text and Atspi.Text.get_n_selections(text) else None
        selection = Atspi.Text.get_text(text, selected.start_offset, selected.end_offset) if selected else ""
        print("History search details:", labels[-3:], snippets, selection)
        raise
    close_find()
    key("ctrl+shift+t")
    wait_for("independent tab for history search", lambda: screen() and "history-search-appended" not in accessible_text(screen()))
    open_find()
    wait_for("history search stays on the current terminal", lambda: first_named(pid, "No matches in retained history."))
    close_find()
    send("exit")
    wait_text("history-search-appended")
    print(f"{app} X11 history search passed: soft wrap, archive append during search, older page, exact clipboard and tab scope")


def check_keyboard_disambiguation(app, root, send, key, wait_text):
    probe = root / "keyboard-capture.py"
    receipt = root / "keyboard-bytes"
    probe.write_text('''import os, select, sys, termios, tty
mode = termios.tcgetattr(0)
try:
    tty.setraw(0)
    os.write(1, b"\\x1b[?1049h\\x1b[>31u\\x1b[?u\\x1b[2J\\x1b[Hkeyboard-mode-ready")
    with open(sys.argv[1], "wb", buffering=0) as output:
        while select.select([0], [], [], 10)[0]:
            data = os.read(0, 256)
            output.write(data)
            if not data or data.endswith(b"q"):
                break
finally:
    os.write(1, b"\\x1b[?1049l\\r\\nkeyboard-mode-finished\\r\\n")
    termios.tcsetattr(0, termios.TCSANOW, mode)
''')
    send(f"/usr/bin/python3 {shlex.quote(str(probe))} {shlex.quote(str(receipt))}")
    wait_text("keyboard-mode-ready")
    key("shift+Return", "Escape", "ctrl+i", "q")
    wait_text("keyboard-mode-finished")
    expected = b"\x1b[?1u\x1b[13;2u\x1b[27u\x1b[105;5uq"
    actual = receipt.read_bytes()
    assert actual == expected, (expected, actual)
    print(f"{app} X11 keyboard passed: negotiated disambiguation, Shift+Enter, Escape, Ctrl+I and ordinary text")


def check_hidden_output(app, pid, root, seconds, send, key, screen, wait_text):
    probe = root / "hidden-output.py"
    gate = root / "start-hidden-output"
    receipt = root / "hidden-drain.json"
    probe.write_text('''import json, os, select, sys, termios, time, tty
from pathlib import Path
gate, receipt, seconds = Path(sys.argv[1]), Path(sys.argv[2]), int(sys.argv[3])
mode = termios.tcgetattr(0)
batch = b"hidden-output-row " + b"x" * 60 + b"\\r\\n"
payload = batch * 64 + b"\\x1b[5n"
try:
    tty.setraw(0)
    os.write(1, b"\\r\\nhidden-output-ready\\r\\n")
    deadline = time.monotonic() + 15
    while not gate.exists():
        if time.monotonic() >= deadline:
            raise TimeoutError("tab was never hidden")
        time.sleep(0.02)
    started = time.monotonic()
    count = 0
    while time.monotonic() - started < seconds:
        pending = memoryview(payload)
        while pending:
            pending = pending[os.write(1, pending):]
        reply = b""
        deadline = time.monotonic() + 2
        while len(reply) < 4:
            if not select.select([0], [], [], max(0, deadline - time.monotonic()))[0]:
                raise TimeoutError("hidden PTY reply did not arrive")
            data = os.read(0, 4 - len(reply))
            if not data:
                raise EOFError("hidden PTY was closed")
            reply += data
        if reply != b"\\x1b[0n":
            raise RuntimeError(f"unexpected reply at batch {count}")
        count += 1
        time.sleep(0.06)
    os.write(1, b"\\r\\nhidden-output-tail\\r\\n")
    temporary = receipt.with_suffix(".tmp")
    temporary.write_text(json.dumps({"seconds": time.monotonic() - started,
                                     "verified_replies": count,
                                     "output_bytes": count * len(payload)}))
    temporary.replace(receipt)
finally:
    termios.tcsetattr(0, termios.TCSANOW, mode)
''')
    send(f"/usr/bin/python3 {shlex.quote(str(probe))} "
         f"{shlex.quote(str(gate))} {shlex.quote(str(receipt))} {seconds}")
    wait_text("hidden-output-ready")
    key("ctrl+shift+t")
    wait_for("the second ordinary tab hides its running sibling",
             lambda: screen() and "hidden-output-ready" not in accessible_text(screen()))
    gate.touch()
    status_path = Path(f"/proc/{pid}/status")
    samples = []
    deadline = time.monotonic() + seconds + 10
    while not receipt.exists():
        if time.monotonic() > deadline:
            raise RuntimeError("hidden PTY did not finish its replies within the test duration")
        resident = re.search(r"(?m)^VmRSS:\s+(\d+) kB", status_path.read_text())
        if resident is not None:
            samples.append(int(resident[1]))
        time.sleep(0.25)
    result = json.loads(receipt.read_text())
    assert result["seconds"] >= seconds and result["verified_replies"] > 0, result
    send("exit")
    wait_text("hidden-output-tail")
    print(f"{app} X11 hidden PTY passed: {result['verified_replies']} ordered replies, "
          f"{result['output_bytes']} bytes, {result['seconds']:.1f} s, "
          f"sampled RSS {min(samples)}–{max(samples)} KiB and tail after reattach")


def check_scrollback_policy(app, send, key, screen, wait_text):
    send(r"printf '\033[3J\033[2J\033[Hhistory-policy-start\n'; "
         "i=1; while [ \"$i\" -le 1400 ]; do printf 'retained-%04d\\n' \"$i\"; i=$((i+1)); done; "
         r"printf 'history-policy-middle\n'")
    wait_text("history-policy-middle")
    key("shift+Home")
    wait_text("history-policy-start")
    key("shift+End")
    wait_text("history-policy-middle")
    send("i=1401; while [ \"$i\" -le 1800 ]; do printf 'retained-%04d\\n' \"$i\"; i=$((i+1)); done; "
         r"printf 'history-policy-tail\n'")
    wait_text("history-policy-tail")
    key("shift+Home")
    top = wait_for("the configured scrollback evicted its oldest rows", lambda: re.match(
        r"retained-(\d{4})(?: +|\n)", accessible_text(screen())))
    assert 1 < int(top[1]) < 1400, top[1]
    key("shift+End")
    wait_text("history-policy-tail")
    print(f"{app} X11 scrollback passed: configured 1500-row preference, eviction and return to live")


def check_resize_history(app, window, send, key, screen, wait_text):
    size = subprocess.check_output(
        ["xdotool", "getwindowgeometry", "--shell", window], text=True, timeout=5)
    width = int(re.search(r"(?m)^WIDTH=(\d+)$", size)[1])
    height = int(re.search(r"(?m)^HEIGHT=(\d+)$", size)[1])

    def resize(new_height):
        subprocess.run(["xdotool", "windowsize", "--sync", window, str(width), str(new_height)],
                       check=True, timeout=5)

    def display():
        return accessible_text(screen())

    send(r"printf '\033[3J\033[2J\033[Hresize-history-top\033[999;1Hresize-history-live'")
    initial = wait_for("actual top and bottom rows before resize", lambda: (
        text if (text := display()).startswith("resize-history-top") and "resize-history-live" in text
        else None))
    old_rows = len(initial.splitlines())
    resize(max(320, height - 240))
    wait_for("physical height shrink preserved the live row", lambda: (
        len(text.splitlines()) < old_rows and "resize-history-live" in text
        if (text := display()) else False))
    key("shift+Home")
    wait_text("resize-history-top")
    key("shift+End")
    wait_for("returning to the resized live viewport", lambda: "resize-history-live" in display())
    resize(height)
    wait_for("original viewport height restored", lambda: len(display().splitlines()) >= old_rows)
    print(f"{app} X11 resize passed: top rows retained in history and current shell output stayed visible")


def check_history_reflow(app, root, window, send, key, screen, wait_text):
    source = "reflow-source-" + "".join(f"[{index:03}]" for index in range(104))
    fixture = root / "history-reflow.txt"
    fixture.write_text(source + "\n")

    def tool(*args):
        subprocess.run(["xdotool", *args], check=True, timeout=5)

    def resize(width):
        tool("windowsize", "--sync", window, str(width), "760")

    def display():
        return accessible_text(screen()).splitlines()

    def logical_line(required_width=None):
        text = display()
        count = next((index for index, line in enumerate(text) if line.startswith("reflow-filler-")), 0)
        rows = [line.rstrip(" ") for line in text[:count]]
        if (rows and "".join(rows) == source
                and (required_width is None or len(rows[0]) < required_width)):
            return rows
        return None

    resize(1120)
    send(r"printf '\033[3J\033[2J\033[H'; cat " + shlex.quote(str(fixture))
         + r"""; awk 'BEGIN {for (i=1; i<=120; i++) printf "reflow-filler-%03d\n", i}'""")
    wait_text("reflow-filler-120")
    key("shift+Home")
    wide = wait_for("full archived line in the wide viewport", logical_line)
    wide_columns = len(wide[0])
    resize(640)
    narrow = wait_for("same archived line reflowed at the smaller width", lambda: logical_line(wide_columns))
    assert len(narrow) > len(wide)

    bounds = screen().get_component_iface().get_extents(Atspi.CoordType.WINDOW)
    tool("mousemove", "--window", window, str(bounds.x + bounds.width // 2),
         str(bounds.y + bounds.height // 2), "click", "5")

    def shows(first):
        text = display()
        return bool(text) and text[0].rstrip(" ") == first

    wait_for("wheel advances three reflowed rows", lambda: shows(narrow[3]))
    raw_offset = sum(map(len, narrow[:3]))
    wide_start = raw_offset // wide_columns * wide_columns
    resize(1120)
    wait_for("the same retained source stays visible after widening",
             lambda: shows(source[wide_start:wide_start + wide_columns]))
    resize(640)
    wait_for("original reading position restored at the smaller width", lambda: shows(narrow[3]))
    key("shift+End")
    wait_text("reflow-filler-120")
    print(f"{app} X11 history reflow passed: {wide_columns} to {len(narrow[0])} columns, "
          "exact logical output and stable position through a round-trip resize")


def check_streamed_paste(app, root, send, key, wait_text):
    probe = root / "paste-capture.py"
    receipt = root / "paste-bytes"
    payload = "native-paste-界\r\n".encode() * 2800
    contents = payload.replace(b"\r\n", b"\r")
    expected = b"\x1b[200~" + contents + b"\x1b[201~\x1b[0n"
    probe.write_text('''import os, select, sys, termios, tty
mode = termios.tcgetattr(0)
try:
    tty.setraw(0)
    os.write(1, b"\\x1b[?2004h\\r\\nstream-paste-ready\\r\\n")
    remaining = int(sys.argv[2])
    first = True
    with open(sys.argv[1], "wb", buffering=0) as output:
        while remaining > 0 and select.select([0], [], [], 10)[0]:
            data = os.read(0, min(remaining, 4096))
            if not data:
                break
            output.write(data)
            remaining -= len(data)
            if first:
                os.write(1, b"\\x1b[5n")
                first = False
    if remaining:
        raise RuntimeError("paste or terminal reply was incomplete")
finally:
    os.write(1, b"\\x1b[?2004l\\r\\nstream-paste-finished\\r\\n")
    termios.tcsetattr(0, termios.TCSANOW, mode)
''')
    send(f"/usr/bin/python3 {shlex.quote(str(probe))} "
         f"{shlex.quote(str(receipt))} {len(expected)}")
    wait_text("stream-paste-ready")
    clipboard = root / "large-clipboard.txt"
    clipboard.write_bytes(payload)
    with subprocess.Popen(["xclip", "-quiet", "-selection", "clipboard", "-in", str(clipboard)],
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as owner:
        try:
            wait_for("X11 clipboard owns the full paste", lambda: subprocess.check_output(
                ["xclip", "-selection", "clipboard", "-out"], stderr=subprocess.DEVNULL,
                timeout=5) == payload)
            key("ctrl+shift+v")
            wait_text("stream-paste-finished")
        finally:
            if owner.poll() is None:
                owner.terminate()
    actual = receipt.read_bytes()
    assert actual == expected, f"PTY saw {len(actual)} of {len(expected)} bytes or reordered reply"
    print(f"{app} X11 paste passed: {len(payload)} clipboard bytes, one bracket pair and an ordered terminal reply")


def check_tui(app, root, window, send, key, screen, wait_text):
    send(r'''printf 'terminfo=[%s] color=[%s]\n' "$TERM" "$COLORTERM"''')
    wait_text("terminfo=[xterm-256color] color=[truecolor]")
    probe = root / "pointer-capture.py"
    receipt = root / "pointer-bytes"
    probe.write_text('''import os, select, sys, termios, tty
mode = termios.tcgetattr(0)
try:
    tty.setraw(0)
    os.write(1, b"\\x1b[?1049h\\x1b[2J\\x1b[?1002;1004;1006h\\x1b[2;1Hp"
                b"\\x1b[48;2;18;227;113mo\\x1b[0min\\x1b[48;2;227;18;113mt\\x1b[0mer-witness")
    with open(sys.argv[1], "wb", buffering=0) as output:
        while select.select([0], [], [], 20)[0]:
            data = os.read(0, 4096)
            output.write(data)
            if not data or data.endswith(b"q"):
                break
finally:
    os.write(1, b"\\x1b[?1002;1004;1006;1049l\\r\\npointer-finished\\r\\n")
    termios.tcsetattr(0, termios.TCSANOW, mode)
''')
    send(r"printf '\033[2J\033[Hprimary-tui-witness\n'")
    wait_text("primary-tui-witness")
    send(f"/usr/bin/python3 {shlex.quote(str(probe))} {shlex.quote(str(receipt))}")
    wait_text("pointer-witness")
    view = screen()
    painted_text = accessible_text(view)
    assert "primary-tui-witness" not in painted_text
    caret = painted_text.index("pointer-witness") + len("pointer-witness")
    assert Atspi.Text.get_caret_offset(view.get_text_iface()) == caret
    wait_for("raw PTY capture file", receipt.exists)

    def tool(*args):
        subprocess.run(["xdotool", *args], check=True, timeout=5)

    bounds = screen().get_component_iface().get_extents(Atspi.CoordType.WINDOW)
    width, height = min(bounds.width, 250), min(bounds.height, 100)

    def move(color):
        def painted():
            return [index for index, pixel in enumerate(sample_pixels(window, bounds, width, height)) if pixel == color]
        pixels = wait_for(f"painted mouse cell {color}", painted)
        x = sum(i % width for i in pixels) // len(pixels)
        y = sum(i // width for i in pixels) // len(pixels)
        tool("mousemove", "--window", window, str(bounds.x + x), str(bounds.y + y))

    move((18, 227, 113))
    tool("mousedown", "1")
    press = b"\x1b[<0;2;2M"
    def wait_input(description, expected):
        try:
            wait_for(description, lambda: expected in receipt.read_bytes())
        except RuntimeError as error:
            raise RuntimeError(f"Expected {expected!r}, received {receipt.read_bytes()!r}") from error

    wait_input("SGR mouse press at the second cell", press)
    # Move within the pressed cell and order a keyboard receipt after the
    # motion. Cell-based tracking must stay quiet until the cell changes.
    tool("mousemove_relative", "--sync", "--", "1", "0")
    tool("key", "z")
    cell_witness = b"z"
    wait_input("same-cell drag suppression before the keyboard receipt", press + cell_witness)
    move((227, 18, 113))
    drag = b"\x1b[<32;5;2M"
    wait_input("SGR mouse drag at the fifth cell", drag)
    tool("mouseup", "1", "click", "4")
    mouse = press + cell_witness + drag + b"\x1b[<0;5;2m\x1b[<64;5;2M"
    wait_input("paired SGR release and wheel bytes", mouse)
    tool("keydown", "Shift_L", "click", "1", "keyup", "Shift_L")
    key("ctrl+shift+t")
    wait_for("new terminal while the TUI is unfocused", lambda: screen() and "pointer-witness" not in accessible_text(screen()))
    send("exit")
    wait_text("pointer-witness")
    key("q")
    wait_text("pointer-finished")
    wait_text("primary-tui-witness")
    assert receipt.read_bytes() == mouse + b"\x1b[O\x1b[Iq", receipt.read_bytes()

    vim = shutil.which("vim") or shutil.which("vim.tiny")
    assert vim, "vim or vim.tiny is required for the terminal fixture"
    document = root / "terminal-vim-fixture.txt"
    document.write_text("vim-edit-witness\n")
    send(f"{shlex.quote(vim)} -Nu NONE -n -i NONE {shlex.quote(str(document))}")
    wait_text("vim-edit-witness")
    key("0", "i")
    tool("type", "--clearmodifiers", "verified-")
    key("Escape")
    send(":wq")
    wait_text("pointer-finished")
    assert document.read_text() == "verified-vim-edit-witness\n"
    print(f"{app} X11 TUI passed: exact SGR press/drag/release/wheel, Shift selection, focus, and Vim save")


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
        check_terminal_palette(app, root, window, type_command, command, output, screen_extents)
    if osc8_links:
        check_osc8_links(app, pid, root, window, type_command, command, output, screen_extents)
    # Give the grapheme a known place in history, independently of the shell's
    # prompt depth and the viewport used by the scrolling journey above.
    cluster_line = "cluster-fixture: e\u0301 👩‍💻"
    cluster_bytes = "".join(f"\\{byte:03o}" for byte in "👩‍💻".encode())
    type_command(r"printf '\033[2J\033[3J\033[Hcluster-fixture: e'; sleep .05; printf '\314\201 "
                 + cluster_bytes + r"\n'")
    output(r"(?m)^" + re.escape(cluster_line) + r" *$")
    type_command(r"""awk 'BEGIN {for (i=1; i<=120; i++) print "cluster-history-" i}'""")
    output(r"(?m)^cluster-history-120 *$")
    assert cluster_line not in read_screen()
    command("key", "shift+Home")
    output(r"(?m)^" + re.escape(cluster_line) + r" *$")


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


def check_terminal_palette(app, root, window, type_command, command, output, screen_extents):
    receipt = root / "palette-query-bytes"
    background = (16, 32, 48)
    indexed = (18, 227, 113)
    cursor = (255, 95, 28)
    expected = (b"\x1b]11;rgb:1010/2020/3030\x07\x1b[0n"
                b"\x1b]4;42;rgb:1212/e3e3/7171\x1b\\\x1b]12;rgb:ffff/5f5f/1c1c\x07")
    type_command(
        r"stty raw -echo; printf '\033[2J\033[3J\033[H\033]11;#102030\007"
        r"\033]4;42;#12e371\007\033]12;#ff5f1c\007\033[48;5;42mpalette-indexed\033[0m'"
        r"; printf '\r\npalette-default\033[3;1H\033]11;?\007\033[5n\033]4;42;?\033\\\033]12;?\007'; "
        + f"head -c {len(expected)} > {shlex.quote(str(receipt))}; "
        + r"head -c 1 > /dev/null; printf '\033]104;42\007\033]111\007\033]112\007'; stty sane"
    )
    output(r"(?m)^palette-default")
    wait_for("ordered OSC and DSR replies reached the real PTY", lambda: (
        receipt.is_file() and receipt.read_bytes() == expected))
    bounds = wait_for("palette capture viewport", screen_extents)
    width, height = min(bounds.width, 500), min(bounds.height, 100)

    def shows_palette():
        pixels = sample_pixels(window, bounds, width, height)
        return all(pixels.count(color) >= 20 for color in (background, indexed, cursor))

    wait_for("default background, indexed cell and cursor colors rendered", shows_palette)
    command("key", "x")
    wait_for("palette reset repainted existing cells", lambda: not any(
        color in sample_pixels(window, bounds, width, height) for color in (background, indexed, cursor)))
    assert receipt.read_bytes() == expected
    output(r"(?m)^palette-indexed")
    print(f"{app} X11 palette passed: exact OSC and DSR replies plus default, indexed and cursor pixels through reset")


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
    width = bounds.width
    height = min(bounds.height, 200)
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
    command("key", "shift+End")
    prefix = "https://example.test/wrapped/" + "x" * 280
    plain = prefix + "tail"
    type_command(r"printf '\033[2J\033[H(%s\033[4;58;2;18;227;113mtail\033[0m).\r\n' "
                 + shlex.quote(prefix))
    output(r"(?m)^\(https://example\.test/wrapped/")
    click(approved)
    wait_for("exact plain URL across soft wraps", lambda: targets() == [live, live, plain])
    print(f"{app} X11 links passed: live and retained OSC 8 targets and an exact plain URL across soft wraps")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", choices=("forge", "anvil"))
    parser.add_argument("binary", type=Path)
    parser.add_argument("--shell", choices=("sh", "bash"), default="sh",
                        help="verify a specific configured shell")
    parser.add_argument("--underline-colors", action="store_true", help="compare underline pixels with ImageMagick import")
    parser.add_argument("--osc8-links", action="store_true", help="verify OSC 8 clicks through an isolated local URI handler")
    parser.add_argument("--shared-runtime", action="store_true", help="use and verify the common PTY and terminal owner")
    parser.add_argument("--ordinary-panes", action="store_true", help="exercise the normal tab and split actions")
    parser.add_argument("--tui", action="store_true", help="exercise negotiated pointer and focus input and a real Vim edit")
    parser.add_argument("--keyboard-disambiguation", action="store_true",
                        help="verify supported Kitty keyboard flags and exact key bytes")
    parser.add_argument("--streamed-paste", action="store_true", help="verify a large clipboard paste and ordered reply")
    parser.add_argument("--scrollback-policy", action="store_true", help="verify the configured history row limit")
    parser.add_argument("--resize-history", action="store_true", help="resize the real window and retain displaced rows")
    parser.add_argument("--history-search", action="store_true", help="search and copy real retained history")
    parser.add_argument("--hidden-output-seconds", type=int, default=0,
                        help="verify PTY drain and replies while its tab is hidden")
    args = parser.parse_args()
    if not args.ordinary_panes and any((args.tui, args.keyboard_disambiguation, args.streamed_paste,
                                        args.scrollback_policy, args.resize_history, args.history_search,
                                        args.hidden_output_seconds)):
        parser.error("the extended keyboard, clipboard and history probes require --ordinary-panes")
    if not 0 <= args.hidden_output_seconds <= 600 or (args.hidden_output_seconds and not args.ordinary_panes):
        parser.error("--hidden-output-seconds accepts 0..600 and requires --ordinary-panes")
    start_accessibility()
    with tempfile.TemporaryDirectory(prefix=f"{args.app}-shared-terminal-") as directory:
        run(args, Path(directory))


if __name__ == "__main__":
    main()
