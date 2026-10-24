#!/usr/bin/env python3
"""Measure real Vim input-to-semantic-frame latency; this is not a GUI FPS test.

The legacy full-frame transport is intentional: source extraction costs must
be measured separately from render_delta wire savings. Run from this folder.
Every measurement includes real PTY input, Vim's 16 ms coalescing timer,
syntax extraction, serialization, loopback delivery and Python JSON decode.
"""

from __future__ import annotations

import argparse
import collections
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

from benchmark_metrics import distribution, scroll_frame_matches, validate_workload
from test_bridge import Editor, VIM
import test_bridge


def process_cpu_seconds(pid):
    """Linux child CPU ticks; unavailable platforms still report wall time."""
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rpartition(")")[2].split()
        return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")
    except (OSError, ValueError, IndexError):
        return None


def measure(lines: int, samples: int, windows: int):
    validate_workload(lines, samples, windows)
    with tempfile.TemporaryDirectory(prefix="jterm-vim9-bench-") as directory:
        root = Path(directory)
        source = root / "large.rs"
        source.write_text("".join(f'let value = "中文é😀"; // sample {i:06d}\n' for i in range(lines)), encoding="utf-8")
        e = Editor(root)
        try:
            e.request("open", path=str(source))
            e.state_where(lambda s: s["file"] == str(source))
            e.ex("syntax on | setfiletype rust")
            for _ in range(windows - 1):
                e.request("split", vertical=True)
            start = time.perf_counter()
            frame = e.gui_frame()
            first_ms = (time.perf_counter() - start) * 1000
            if not frame.get("supported"):
                raise AssertionError(frame.get("fallback_reason"))
            if len(frame.get("windows", [])) != windows:
                raise AssertionError("Wrong window count")
            cached = []
            for _ in range(min(samples, 10)):
                start = time.perf_counter()
                frame = e.gui_frame()
                cached.append((time.perf_counter() - start) * 1000)
            scrolling = []
            frame_bytes = []
            run_counts = []
            cpu_before = process_cpu_seconds(e.process.pid)
            for _ in range(samples):
                previous = frame
                e.messages = collections.deque(m for m in e.messages if m.get("event") != "render")
                start = time.perf_counter()
                e.keys(b"\x05")  # Vim Ctrl-E: scroll the active window by one line.
                frame = e.wait(lambda m: scroll_frame_matches(m, previous))
                scrolling.append((time.perf_counter() - start) * 1000)
                if not frame.get("supported"):
                    raise AssertionError(frame.get("fallback_reason"))
                frame_bytes.append(len(json.dumps(frame, ensure_ascii=False, separators=(",", ":")).encode("utf-8")))
                run_counts.append(len(frame["runs"]))
            cpu_after = process_cpu_seconds(e.process.pid)
            return {
                "buffer_lines": lines,
                "buffer_bytes": source.stat().st_size,
                "windows": windows,
                "pty_rows": frame["rows"],
                "pty_columns": frame["columns"],
                "syntax": "Vim runtime rust",
                "initial_forced_frame_ms": round(first_ms, 3),
                "cached_forced_frame": distribution(cached),
                "ctrl_e_to_semantic_frame": distribution(scrolling),
                "vim_cpu_ms_per_scroll": round((cpu_after - cpu_before) * 1000 / samples, 3)
                if cpu_before is not None and cpu_after is not None else None,
                "full_frame_bytes_min": min(frame_bytes),
                "full_frame_bytes_max": max(frame_bytes),
                "visible_runs_max": max(run_counts),
                "last_viewports": [{k: w[k] for k in ("window", "topline", "botline")} for w in frame["windows"]],
            }
        finally:
            e.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lines", type=int, default=100_000)
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--windows", type=int, nargs="+", default=[1, 2])
    parser.add_argument("--runtime", type=Path, help="Explicit bridge.vim for controlled runtime comparisons")
    args = parser.parse_args()
    if not VIM:
        parser.error("Install Vim 9 or set VIM9_TEST_VIM")
    try:
        for windows in args.windows:
            validate_workload(args.lines, args.samples, windows)
    except ValueError as error:
        parser.error(str(error))
    if args.runtime:
        if not args.runtime.is_file():
            parser.error("--runtime must name an existing bridge.vim file")
        test_bridge.RUNTIME = args.runtime.resolve()
    result = {
        "metric": "input-to-semantic-frame, not displayed frame time or FPS",
        "transport": "legacy full render frames (render_protocol 2 not negotiated)",
        "percentile_method": "nearest-rank: ceil(0.95 * samples)",
        "sample_validation": "active source advances exactly one line; sibling viewports remain unchanged",
        "runtime": str(test_bridge.RUNTIME),
        "vim": subprocess.check_output([str(VIM), "--version"], text=True).splitlines()[0],
        "results": [measure(args.lines, args.samples, w) for w in args.windows],
    }
    print(json.dumps(result, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
