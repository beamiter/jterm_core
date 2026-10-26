#!/usr/bin/env python3
"""Isolated native X11 visual acceptance for the four transcript canvases.

Uses real shell/OSC integration and X11 wheel events. Captures actual displayed
pixels; it is functional software-rendering evidence, not a hardware benchmark.
This opt-in acceptance diagnostic is not installed with the applications.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import subprocess
import time

from PIL import ImageGrab
import numpy as np


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("app", choices=["anvil", "forge", "ember", "frost"])
    parser.add_argument("mode", choices=["block", "chat"])
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    task_home = out / "home"
    task_home.mkdir()
    runtime = out / "runtime"
    runtime.mkdir(mode=0o700)
    env = os.environ.copy()
    for key in ("DISPLAY", "WAYLAND_DISPLAY", "SWAYSOCK", "DBUS_SESSION_BUS_ADDRESS"):
        env.pop(key, None)
    env.update(
        HOME=str(task_home),
        XDG_CONFIG_HOME=str(task_home / ".config"),
        XDG_DATA_HOME=str(task_home / ".local/share"),
        XDG_STATE_HOME=str(task_home / ".local/state"),
        XDG_CACHE_HOME=str(task_home / ".cache"),
        XDG_RUNTIME_DIR=str(runtime),
        LIBGL_ALWAYS_SOFTWARE="1",
        GDK_BACKEND="x11",
        GSK_RENDERER="cairo",
        ICED_BACKEND="tiny-skia",
        RUST_LOG="warn",
        SHELL="/bin/bash",
        LANG="C.UTF-8",
        CANVAS_PROBE_READY=str(out / "shell-ready"),
    )
    config_dir = Path(env["XDG_CONFIG_HOME"]) / args.app
    config_dir.mkdir(parents=True)
    config = (
        'shell = "/bin/bash"\n'
        f'terminal_mode = "{args.mode}"\n'
        'restore_session = false\n'
        'initial_width = 1100\ninitial_height = 760\n'
        'opacity = 1.0\nascii_organism_enabled = false\n'
    )
    if args.app in ("anvil", "forge"):
        config += 'font = "DejaVu Sans Mono 13"\n'
    if args.app == "ember":
        config += 'app_renderer = "glow"\n'
    (config_dir / "config.toml").write_text(config)
    shell_profile = r'''export HISTFILE=/dev/null
set -o history
unset HISTCONTROL HISTIGNORE HISTTIMEFORMAT
__canvas_running=0
__canvas_ready=0
__canvas_sequence=0
canvas_sample() {
    printf '\033[38;2;241;88;200mCANVAS ANCHOR %02d  continuous native transcript\033[0m\n' "$1"
}
__canvas_start() {
    [[ $__canvas_ready == 1 && $BASH_COMMAND != __canvas_prompt ]] || return
    __canvas_ready=0
    __canvas_running=1
    ((__canvas_sequence++)) || :
    local command encoded='' ch i LC_ALL=C
    command=$(builtin history 1)
    command=${command#*[0-9]  }
    for ((i=0; i<${#command}; i++)); do
        ch=${command:i:1}
        case $ch in
            [a-zA-Z0-9._~-]) encoded+=$ch ;;
            *) printf -v encoded '%s%%%02X' "$encoded" "'$ch" ;;
        esac
    done
    printf '\e]133;C;id=canvas-%s;cmdline_url=%s\a' "$__canvas_sequence" "$encoded"
}
__canvas_prompt() {
    local status=$?
    if [[ $__canvas_running == 1 ]]; then
        printf '\e]133;D;%s;id=canvas-%s\a' "$status" "$__canvas_sequence"
    fi
    __canvas_running=0
    printf '\e]133;A\a'
    __canvas_ready=1
    printf '%s\n' "$__canvas_sequence" > "$CANVAS_PROBE_READY"
}
PROMPT_COMMAND=__canvas_prompt
PS0=''
PS1='canvas> \[\e]133;B\a\]'
trap '__canvas_start' DEBUG
'''
    for name in (".bashrc", ".bash_profile"):
        (task_home / name).write_text(shell_profile)

    processes = []
    logs = []
    result = {"app": args.app, "mode": args.mode, "renderer": "Xvfb/software",
              "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest()}
    def run(*command, check=True):
        return subprocess.run(command, env=env, capture_output=True, text=True,
                              timeout=10, check=check)
    def capture(name=None):
        frame = ImageGrab.grab(xdisplay=env["DISPLAY"])
        if name:
            frame.save(out / (name + ".png"))
        return frame
    def wait_for_prompt(sequence):
        deadline = time.monotonic() + 10
        ready = out / "shell-ready"
        while time.monotonic() < deadline:
            if ready.exists() and ready.read_text().strip() == str(sequence):
                return
            time.sleep(0.02)
        raise AssertionError(f"shell did not finish command {sequence}; see app.log")
    def marker_mask(frame):
        a = np.asarray(frame)[90:600, 140:940, :3].astype(np.int16)
        return ((a[:, :, 0] > 165) & (a[:, :, 2] > 120) &
                (a[:, :, 0] - a[:, :, 1] > 65) &
                (a[:, :, 2] - a[:, :, 1] > 40))
    def marker_signature(frame):
        pink = marker_mask(frame)
        return hashlib.sha256(pink.tobytes()).hexdigest(), int(pink.sum())
    def first_marker_y(frame):
        a = np.asarray(frame)[45:680, :1100, :3].astype(np.int16)
        pink = ((a[:, :, 0] > 165) & (a[:, :, 2] > 120) &
                (a[:, :, 0] - a[:, :, 1] > 65) &
                (a[:, :, 2] - a[:, :, 1] > 40))
        rows = np.flatnonzero(pink.sum(axis=1) >= 12)
        assert len(rows), "first short-history output is visibly rendered"
        return int(rows[0]) + 45
    try:
        log = (out / "xvfb.log").open("w")
        logs.append(log)
        server = subprocess.Popen(["Xvfb", "-displayfd", "1", "-screen", "0",
                                   "1280x900x24", "-nolisten", "tcp"],
                                  stdout=subprocess.PIPE, stderr=log, text=True)
        processes.append(server)
        assert select.select([server.stdout], [], [], 10)[0], "Xvfb readiness"
        number = server.stdout.readline().strip()
        assert number.isdigit(), "Xvfb display number"
        env["DISPLAY"] = ":" + number
        log = (out / "app.log").open("w")
        logs.append(log)
        app = subprocess.Popen([str(args.binary.resolve())], env=env, cwd=out,
                               stdout=log, stderr=log)
        processes.append(app)
        deadline = time.monotonic() + 20
        window = None
        while time.monotonic() < deadline:
            assert app.poll() is None, "application exited; see app.log"
            reply = run("xdotool", "search", "--onlyvisible", "--pid", str(app.pid), check=False)
            if reply.returncode == 0 and reply.stdout.strip():
                window = reply.stdout.splitlines()[-1]
                break
            time.sleep(0.1)
        assert window, "mapped app window"
        run("xdotool", "windowmove", window, "0", "0")
        run("xdotool", "windowsize", window, "1100", "760")
        run("xdotool", "windowfocus", "--sync", window)
        wait_for_prompt(0)
        time.sleep(0.7)
        capture("startup")
        short_positions = []
        for i in range(18):
            command = f"canvas_sample {i}"
            run("xdotool", "type", "--clearmodifiers", "--delay", "10", "--", command)
            run("xdotool", "key", "--clearmodifiers", "Return")
            wait_for_prompt(i + 1)
            time.sleep(0.09)
            if i < 2:
                time.sleep(0.35)
                frame = capture(f"short-{i + 1}-blocks")
                short_positions.append(first_marker_y(frame))
        result["short_history_first_output_y"] = short_positions
        assert short_positions[0] == short_positions[1], (
            "adding the second short block moved the first block's canvas origin: "
            f"{short_positions}"
        )
        time.sleep(0.8)
        before = capture("before-wheel")
        before_signature, marker_pixels = marker_signature(before)
        assert marker_pixels > 30, "synthetic history is visibly rendered"
        marker_y, marker_x = np.nonzero(marker_mask(before))
        point = len(marker_y) // 2
        pointer_x, pointer_y = int(marker_x[point]) + 140, int(marker_y[point]) + 90
        result["wheel_pointer"] = [pointer_x, pointer_y]
        run("xdotool", "mousemove", str(pointer_x), str(pointer_y))
        frames = []
        samples = []
        start = time.monotonic()
        # xdotool click sleeps 100 ms after dispatch, hiding the first frames.
        run("xdotool", "mousedown", "4", "mouseup", "4")
        for i in range(35):
            frame = capture()
            signature, count = marker_signature(frame)
            frames.append(frame)
            samples.append({"seconds": time.monotonic() - start,
                            "signature": signature, "marker_pixels": count})
            time.sleep(1 / 90)
        frames[0].save(out / "wheel-first.png")
        frames[-1].save(out / "wheel-settled.png")
        frames[0].save(out / "wheel.gif", save_all=True, append_images=frames[1:],
                       duration=18, loop=0)
        unique = len(set(sample["signature"] for sample in samples))
        result.update(marker_pixels=marker_pixels,
                      distinct_wheel_positions=unique,
                      wheel_changed=samples[-1]["signature"] != before_signature,
                      samples=samples)
        assert result["wheel_changed"], "real wheel moved visible history"
        assert unique >= 4, f"wheel needs intermediate painted positions, observed {unique}"
        assert samples[-1]["marker_pixels"] > 30, "no blank frame after scrolling"
        time.sleep(0.2)
        settled = marker_signature(capture("rest"))[0]
        result["stable_after_settle"] = settled == samples[-1]["signature"]
        assert result["stable_after_settle"], "canvas must keep its settled position"
        result["status"] = "pass"
    except Exception as error:
        result.update(status="fail", error=str(error))
        try:
            capture("failure")
        except Exception:
            pass
        raise
    finally:
        for proc in reversed(processes):
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
        for log in logs:
            log.close()
        (out / "result.json").write_text(json.dumps(result, indent=2))
        print(json.dumps({k: v for k, v in result.items() if k != "samples"}, indent=2))


if __name__ == "__main__":
    main()
