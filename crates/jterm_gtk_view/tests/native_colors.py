#!/usr/bin/env python3
"""Check OSC replies and cursor, palette and paper pixels on real app PTYs."""
import argparse
from collections import Counter
import json
import os
from pathlib import Path
import tempfile

from native import Atspi, accessible_text, application_nodes, sample_pixels, start_accessibility, wait_for
from native_jsh import application, xdotool


DEFAULT_CURSOR = (127, 184, 14)
DEFAULTS = {
    "block": ((213, 222, 236), (14, 22, 33), DEFAULT_CURSOR),
    "unified": ((248, 247, 233), (18, 22, 22), DEFAULT_CURSOR),
}
CHAT_DEFAULTS = {
    "forge": ((37, 52, 75), (254, 254, 254), (83, 121, 209)),
    "anvil": ((40, 52, 76), (250, 251, 255), (92, 126, 240)),
}
INK = (214, 233, 182)
PAPER = (16, 32, 48)
CURSOR = (255, 95, 28)
INDEXED_42 = (18, 227, 113)
INDEXED_43 = (171, 18, 209)
DEFAULT_42 = (0, 215, 135)
DEFAULT_43 = (0, 215, 175)
BEL = b"\x07"
ST = b"\x1b\\"

SHELL = r"""#!/usr/bin/python3
import os
from pathlib import Path
import select
import time
import tty

root = Path(__file__).parent
tty.setraw(0)

prompt = b"\x1b]133;A\x07$ \x1b]133;B\x07"
command = b"\r\n\x1b]133;C;cmdline_url=palette-probe\x07active-palette-ready\r\n"
defaults = b"\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]12;?\x07\x1b]4;42;?\x1b\\"
packets = {
    "initial": defaults,
    "changed": (
        b"\x1b]10;#d6e9b6\x07\x1b]11;#102030\x07\x1b]12;#ff5f1c\x07"
        b"\x1b]4;42;#12e371;43;#ab12d1;43;?\x1b\\"
        b"\x1b[48;5;42mpalette-green\x1b[48;5;43mpalette-purple\x1b[0m\r\n"
        b"\x1b]4;42;?;43;?\x07\x1b[5n\x1b]10;?\x1b\\\x1b]11;?\x07\x1b]12;?\x1b\\"
    ),
    "reset": (
        b"\x1b]104;42;43\x07\x1b]110\x07\x1b]111\x07\x1b]112\x07"
        b"\x1b]4;42;?;43;?\x07\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]12;?\x07"
    ),
    "ris": b"\x1b]4;42;#ff00ff\x07\x1bc" + defaults,
}
os.write(1, prompt)
for phase, packet in packets.items():
    if phase != "initial":
        deadline = time.monotonic() + 10
        while not (root / (phase + "-go")).is_file() and time.monotonic() < deadline:
            time.sleep(0.02)
    os.write(1, packet)
    raw = bytearray()
    while select.select([0], [], [], 0.8 if not raw else 0.35)[0]:
        chunk = os.read(0, 2048)
        if not chunk:
            break
        raw.extend(chunk)
    temporary = root / (phase + "-tmp")
    temporary.write_bytes(raw)
    temporary.rename(root / (phase + "-reply"))
    if phase == "initial":
        os.write(1, command)
while os.read(0, 1):
    pass
"""


def color_reply(code, color, terminator):
    rgb = "/".join(f"{byte:02x}" * 2 for byte in color)
    return f"\x1b]{code};rgb:{rgb}".encode("ascii") + terminator


def run(app, binary, mode, root):
    root.mkdir(mode=0o700)
    shell = root / "shell"
    shell.write_text(SHELL)
    shell.chmod(0o700)
    config = root / f"{app}.toml"
    config.write_text(f'shell = {json.dumps(str(shell))}\nterminal_mode = "{mode}"\n')
    config.chmod(0o600)
    env = {k: v for k, v in os.environ.items() if not k.startswith(("JSH_", "JTERM_SHARED_"))}
    env.pop(f"{app.upper()}_SHELL", None)
    env.pop("BASH_ENV", None)
    for key in ("HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME",
                "XDG_RUNTIME_DIR"):
        folder = root / key
        folder.mkdir(mode=0o700)
        env[key] = str(folder)
    env.update(GDK_BACKEND="x11", GTK_A11Y="atspi", GTK_IM_MODULE="simple", GSK_RENDERER="cairo")
    fg, bg, cursor = CHAT_DEFAULTS[app] if mode == "chat" else DEFAULTS[mode]
    default_replies = (
        color_reply("10", fg, BEL) + color_reply("11", bg, ST) + color_reply("12", cursor, BEL)
    )
    initial = default_replies + color_reply("4;42", DEFAULT_42, ST)
    changed = (
        color_reply("4;43", INDEXED_43, ST)
        + color_reply("4;42", INDEXED_42, BEL) + color_reply("4;43", INDEXED_43, BEL) + b"\x1b[0n"
        + color_reply("10", INK, ST) + color_reply("11", PAPER, BEL) + color_reply("12", CURSOR, ST)
    )
    reset = color_reply("4;42", DEFAULT_42, BEL) + color_reply("4;43", DEFAULT_43, BEL) + default_replies
    with application(binary, config, env, root / "app.log", restore=False) as process:
        def screen():
            return next((node for node in application_nodes(process.pid)
                         if node.get_description() == "Shared active terminal"
                         and node.get_state_set().contains(Atspi.StateType.SHOWING)), None)

        def check_replies(phase, expected):
            if phase != "initial":
                (root / (phase + "-go")).touch()
            receipt = root / (phase + "-reply")
            wait_for(f"{mode} {phase} PTY replies", receipt.is_file)
            assert receipt.read_bytes() == expected, (app, mode, phase, receipt.read_bytes(), expected)

        check_replies("initial", initial)
        wait_for("running shared terminal", lambda: "active-palette-ready" in accessible_text(screen()))
        window = xdotool("search", "--onlyvisible", "--pid", str(process.pid)).split()[0]
        check_replies("changed", changed)
        wait_for("both indexed text spans", lambda: "palette-greenpalette-purple" in accessible_text(screen()))

        def pixels():
            node = screen()
            if node is None:
                return None
            bounds = node.get_component_iface().get_extents(Atspi.CoordType.WINDOW)
            width, height = min(bounds.width, 450), min(bounds.height, 180)
            if width <= 8 or height <= 8:
                return None
            return sample_pixels(window, bounds, width, height), width

        def painted_colors():
            sample = pixels()
            if sample is None:
                return None
            colors, width = sample
            counts = Counter(colors)
            if not all(counts[color] >= 20 for color in (PAPER, INDEXED_42, INDEXED_43, CURSOR)):
                return None
            # The first indexed span starts in column one. Its leftmost
            # painted pixel locates the margin even when the dock clips the
            # accessible rectangle or client-side decorations offset it.
            green = [offset for offset, color in enumerate(colors) if color == INDEXED_42]
            left = min(offset % width for offset in green)
            # Unified can also paint zone chrome in the margin.
            return next(((left - gap, offset // width)
                         for offset in green if offset % width == left
                         for gap in range(1, min(left, 5) + 1) if colors[offset - gap] == PAPER), None)

        x, y = wait_for("paper at the margin and both indexed backgrounds and cursor", painted_colors)
        check_replies("reset", reset)

        def restored_colors():
            sample = pixels()
            if sample is None:
                return False
            colors, width = sample
            if x >= width or y >= len(colors) // width:
                return False
            margin = colors[y * width + x]
            restored_paper = margin != PAPER if (app, mode) == ("forge", "chat") else margin == bg
            counts = Counter(colors)
            return restored_paper and counts[cursor] >= 20 and all(
                counts[color] < 20 for color in (PAPER, INDEXED_42, INDEXED_43, CURSOR))
        wait_for("resets repainted existing cells and margin", restored_colors)
        check_replies("ris", initial)
    print(f"{app} {mode}: exact OSC and DSR replies, palette, cursor and margin pixels, and resets", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", choices=("forge", "anvil"))
    parser.add_argument("binary", type=Path)
    parser.add_argument("--mode", choices=("block", "chat", "unified"), action="append")
    args = parser.parse_args()
    start_accessibility()
    with tempfile.TemporaryDirectory(prefix="native-active-colors-") as directory:
        for mode in args.mode or ("block", "chat", "unified"):
            run(args.app, args.binary.resolve(strict=True), mode, Path(directory) / mode)


if __name__ == "__main__":
    main()
