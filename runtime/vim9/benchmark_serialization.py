#!/usr/bin/env python3
"""Compare old/new Vim JSON encoding paths on frozen, real native paint frames.

This isolates serialization and envelope assembly, not complete rendering,
transport latency, displayed frame time or GUI FPS. Each CPU measurement also
includes a fresh Vim's startup and fixture decoding, amortized over iterations.
Run from any directory; Vim and the frame-capture harness need local sockets.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import statistics
import subprocess
import tempfile

import test_bridge
from test_bridge import Editor, VIM


# Deliberately model only the encoding passes removed by the optimization.
# Prebuilt dictionaries exclude source rendering and row grouping. Old paths
# retain their duplicate JSON encoding; new paths append the numeric envelope.
SERIALIZER = r'''vim9script
var sample = json_decode(join(readfile($JTERM_BENCH_FIXTURES), "\n"))[str2nr($JTERM_BENCH_SAMPLE)]
var modern = sample.modern
var optimized = $JTERM_BENCH_OPTIMIZED == '1'
var sink = 0
var frame = sample.frame
var delta = copy(sample.delta)
var legacy = copy(frame)
legacy.revision = 100
legacy.protocol = 1
legacy.seq = 200
var outgoing = copy(delta)
outgoing.protocol = 1
outgoing.seq = 200
# Check the modeled envelopes before timing the repeated encoding path.
var fullJson = json_encode(frame)
var deltaJson = json_encode(delta)
assert_equal(legacy, json_decode(strpart(fullJson, 0, strlen(fullJson) - 1)
  .. ',"revision":100,"protocol":1,"seq":200}'))
assert_equal(outgoing, json_decode(strpart(deltaJson, 0, strlen(deltaJson) - 1)
  .. ',"protocol":1,"seq":200}'))
if !empty(v:errors)
  cquit
endif
for n in range(str2nr($JTERM_BENCH_ITERATIONS))
  var encoded = json_encode(frame)
  if modern || !optimized
    for row in sample.rows
      sink += strlen(json_encode(row))
    endfor
  endif
  if modern
    var encodedDelta = json_encode(delta)
    if optimized
      sink += strlen(strpart(encodedDelta, 0, strlen(encodedDelta) - 1)
        .. ',"protocol":1,"seq":200}' .. "\n")
    else
      sink += strlen(json_encode(outgoing) .. "\n")
    endif
  elseif optimized
    sink += strlen(strpart(encoded, 0, strlen(encoded) - 1)
      .. ',"revision":100,"protocol":1,"seq":200}' .. "\n")
  else
    sink += strlen(json_encode(legacy) .. "\n")
  endif
endfor
qa!
'''


def capture_fixture(root: Path, windows: int, protocol: int) -> dict:
    root.mkdir()
    editor = Editor(root, authenticate=False)
    try:
        editor.send({"action": "auth", "token": editor.token, "protocol": 1,
                     "render_protocol": protocol})
        editor.wait(lambda message: message.get("event") == "state")
        editor.file.write_text("".join(
            f'let value = "中文é😀"; // sample {line:06d}\n' for line in range(500)),
            encoding="utf-8")
        editor.ex("edit! | syntax on | setfiletype rust")
        if windows == 2:
            editor.request("split", vertical=True)
        frame = editor.gui_frame()
        if not frame["supported"] or len(frame["windows"]) != windows:
            raise RuntimeError(f"Unexpected paint fixture: {frame.get('fallback_reason')}")
        for key in ("seq", "protocol", "revision"):
            frame.pop(key, None)
        rows = [[run for run in frame["runs"] if run["row"] == row]
                for row in range(frame["rows"])]
        delta = {**frame, "event": "render_delta", "revision": 100,
                 "base_revision": 99, "row_updates": []}
        del delta["runs"]
        return {"windows": windows, "modern": protocol == 7,
                "frame": frame, "rows": rows, "delta": delta}
    finally:
        editor.close()


def measure(iterations: int, rounds: int, windows: list[int],
            temp_dir: Path | None = None, fixtures_output: Path | None = None) -> dict:
    with tempfile.TemporaryDirectory(prefix="vim9-serialization-", dir=temp_dir) as directory:
        root = Path(directory)
        fixtures = [capture_fixture(root / f"source-{count}-{protocol}", count, protocol)
                    for count in windows for protocol in (1, 7)]
        fixture_bytes = json.dumps(fixtures, ensure_ascii=False).encode("utf-8")
        fixture_path = root / "fixtures.json"
        fixture_path.write_bytes(fixture_bytes)
        if fixtures_output:
            fixtures_output.write_bytes(fixture_bytes)
        script = root / "serialize.vim"
        script.write_text(SERIALIZER, encoding="utf-8")
        samples = []
        for round_number in range(rounds):
            for index, fixture in enumerate(fixtures):
                order = (False, True) if round_number % 2 == 0 else (True, False)
                for optimized in order:
                    env = {**os.environ, "JTERM_BENCH_FIXTURES": str(fixture_path),
                           "JTERM_BENCH_SAMPLE": str(index),
                           "JTERM_BENCH_OPTIMIZED": str(int(optimized)),
                           "JTERM_BENCH_ITERATIONS": str(iterations)}
                    before = resource.getrusage(resource.RUSAGE_CHILDREN)
                    subprocess.run([str(VIM), "-Nu", "NONE", "-i", "NONE", "-n", "-es",
                                    "-S", str(script)], env=env, check=True,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                    after = resource.getrusage(resource.RUSAGE_CHILDREN)
                    cpu = (after.ru_utime - before.ru_utime
                           + after.ru_stime - before.ru_stime)
                    samples.append({"windows": fixture["windows"], "modern": fixture["modern"],
                                    "optimized": optimized, "round": round_number,
                                    "cpu_us_per_frame": cpu * 1_000_000 / iterations})
        summary = []
        for count in windows:
            for modern in (False, True):
                medians = [statistics.median(sample["cpu_us_per_frame"] for sample in samples
                           if sample["windows"] == count and sample["modern"] == modern
                           and sample["optimized"] == optimized and sample["round"] > 0)
                           for optimized in (False, True)]
                old, new = medians
                summary.append({"windows": count, "protocol": 7 if modern else 1,
                                "old_median_cpu_us": old, "new_median_cpu_us": new,
                                "reduction_percent": 100 * (1 - new / old) if old else None})
        return {
            "scope": __doc__, "measurement": "per-child getrusage user+system CPU",
            "iterations": iterations, "rounds": rounds, "warmup_rounds": 1,
            "ordering": "old/new on even rounds; new/old on odd rounds",
            "workload": "500 Rust source lines, Unicode and combining marks; frozen visible frames",
            "delta": "cursor-only: empty row_updates with unchanged overscan",
            "runtime": str(test_bridge.RUNTIME),
            "runtime_sha256": hashlib.sha256(test_bridge.RUNTIME.read_bytes()).hexdigest(),
            "fixture_sha256": hashlib.sha256(fixture_bytes).hexdigest(),
            "serializer_sha256": hashlib.sha256(SERIALIZER.encode()).hexdigest(),
            "vim": subprocess.check_output([str(VIM), "--version"], text=True).splitlines()[0],
            "samples": samples, "summary": summary,
        }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--iterations", type=int, default=400)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--windows", type=int, nargs="+", choices=(1, 2), default=[1, 2])
    parser.add_argument("--runtime", type=Path, help="Bridge used to capture real paint fixtures")
    parser.add_argument("--temp-dir", type=Path, help="Parent directory for automatically cleaned temporary files")
    parser.add_argument("--fixtures-output", type=Path, help="Optionally retain the exact JSON fixtures")
    args = parser.parse_args()
    if not VIM:
        parser.error("Install Vim 9 or set VIM9_TEST_VIM")
    if args.iterations < 1 or args.rounds < 2:
        parser.error("Use positive --iterations and at least two --rounds (first is warmup)")
    if args.temp_dir and not args.temp_dir.is_dir():
        parser.error("--temp-dir must name an existing directory")
    if args.runtime:
        if not args.runtime.is_file():
            parser.error("--runtime must name an existing bridge.vim file")
        test_bridge.RUNTIME = args.runtime.resolve()
    result = measure(args.iterations, args.rounds, list(dict.fromkeys(args.windows)),
                     args.temp_dir, args.fixtures_output)
    print(json.dumps(result, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
