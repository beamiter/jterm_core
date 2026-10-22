#!/usr/bin/env python3
"""Measure source-bound scroll acknowledgements in real Vim, not GUI FPS.

Run with --runtime to compare another bridge.vim against the same workload.
The active cursor crosses the viewport edge on every measured request; syntax,
relative numbers, CursorLine, Unicode and a stationary sibling remain enabled.
"""

import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import time

from benchmark_render import distribution, process_cpu_seconds
import test_bridge
from test_bridge import Editor, VIM


def measure(samples, windows, lines):
    with tempfile.TemporaryDirectory(prefix="vim9-scroll-bench-") as directory:
        root = Path(directory)
        editor = Editor(root, authenticate=False)
        try:
            editor.send({"action": "auth", "token": editor.token, "protocol": 1,
                         "render_protocol": 5})
            editor.wait(lambda m: m.get("event") == "state")
            source = root / "large.rs"
            source.write_text("".join(
                f'let value_{n:05d} = "中文é😀"; // a syntax highlighted line\n'
                for n in range(lines)), encoding="utf-8")
            editor.request("open", path=str(source))
            editor.ex("syntax on | setfiletype rust | set number relativenumber cursorline scrolloff=5")
            for _ in range(windows - 1):
                editor.request("split", vertical=True)
            editor.ex("normal! 100Gzt")
            frame = editor.gui_frame()
            if not frame["supported"]:
                raise AssertionError(frame.get("fallback_reason"))
            active = frame["window"]
            window = next(w for w in frame["windows"] if w["window"] == active)
            previous_cursor = window["cursor"].get("line")
            cursor_moves = 0
            durations = []
            cpu_before = process_cpu_seconds(editor.process.pid)
            for _ in range(samples):
                start = time.perf_counter()
                reply = editor.request("scroll_view", lines=1, **{
                    key: window[key] for key in
                    ("window", "buffer", "changedtick", "topline", "leftcol")})
                frame = editor.wait(lambda m:
                    m.get("event") in ("render", "render_delta")
                    and any(w.get("scroll_ack") == reply["id"] for w in m.get("windows", [])))
                durations.append((time.perf_counter() - start) * 1000)
                if not frame["supported"] or frame["seq"] >= reply["seq"]:
                    raise AssertionError("Scroll lost its supported, ordered semantic acknowledgement")
                window = next(w for w in frame["windows"] if w["window"] == active)
                cursor = window["cursor"].get("line")
                cursor_moves += cursor != previous_cursor
                previous_cursor = cursor
            cpu_after = process_cpu_seconds(editor.process.pid)
            return {
                "windows": windows,
                "buffer_lines": lines,
                "pty_rows": frame["rows"],
                "pty_columns": frame["columns"],
                "scroll_ack": distribution(durations),
                "cursor_moves": cursor_moves,
                "vim_cpu_ms_per_scroll": round((cpu_after - cpu_before) * 1000 / samples, 3)
                if cpu_before is not None and cpu_after is not None else None,
            }
        finally:
            editor.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime", type=Path)
    parser.add_argument("--samples", type=int, default=160)
    parser.add_argument("--windows", type=int, nargs="+", default=[1, 2])
    parser.add_argument("--lines", type=int, default=100_000)
    args = parser.parse_args()
    if not VIM or args.samples < 1 or args.lines < args.samples + 200:
        parser.error("Install Vim 9, use positive samples and at least samples + 200 lines")
    if any(w not in (1, 2) for w in args.windows):
        parser.error("Use one or two windows")
    if args.runtime:
        if not args.runtime.is_file():
            parser.error("--runtime must name an existing bridge.vim")
        test_bridge.RUNTIME = args.runtime.resolve()
    print(json.dumps({
        "metric": "request to ordered semantic ack/response; not displayed latency or FPS",
        "runtime": str(test_bridge.RUNTIME),
        "vim": subprocess.check_output([VIM, "--version"], text=True).splitlines()[0],
        "results": [measure(args.samples, windows, args.lines) for windows in args.windows],
    }, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
