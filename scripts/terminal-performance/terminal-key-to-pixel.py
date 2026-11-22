#!/usr/bin/env python3
"""Measure a focused X11 terminal from XTEST key press to visible PTY output."""

import argparse
from ctypes import CDLL, POINTER, byref, c_int, c_ubyte, c_uint, c_ulong, c_void_p
import hashlib
from io import BytesIO
import json
import os
from pathlib import Path
import platform
import re
import runpy
import select
import statistics
import subprocess
import sys
import termios
import time
import tty

COLORS = ((17, 223, 87), (229, 39, 137))
GRID = (120, 40)
WARMUP = 5


def inside_terminal(directory, samples):
    def paint(index):
        red, green, blue = COLORS[index]
        output = ''.join(f'\x1b[{row};4H\x1b[48;2;{red};{green};{blue}m' + ' ' * 18
                         for row in range(3, 8)) + '\x1b[0m'
        sys.stdout.buffer.write(output.encode())
        sys.stdout.buffer.flush()

    if (directory / 'pixel-ready').exists():
        raise ValueError('use a fresh witness directory for each run')
    settings = termios.tcgetattr(0)
    try:
        tty.setraw(0)
        if tuple(os.get_terminal_size(0)) != GRID:
            raise ValueError(f'the PTY must be {GRID[0]} columns by {GRID[1]} rows')
        sys.stdout.buffer.write(b'\x1b[?25l\x1b[2J')
        paint(0)
        ready = directory / 'pixel-ready.tmp'
        ready.write_text(json.dumps(dict(grid=GRID, samples=samples)))
        ready.replace(directory / 'pixel-ready')
        for index in range(samples + WARMUP):
            if not select.select([0], [], [], 20)[0]:
                raise TimeoutError(f'missing key event after {index} samples')
            if os.read(0, 1) != b'x':
                raise ValueError(f'unexpected input at sample {index}')
            paint((index + 1) % 2)
        if tuple(os.get_terminal_size(0)) != GRID:
            raise ValueError('terminal geometry changed during measurement')
        (directory / 'pixel-consumed').touch()
        # Keep the last color visible until the observer records it.
        deadline = time.monotonic() + 5
        while not (directory / 'pixel-complete').exists() and time.monotonic() < deadline:
            time.sleep(0.01)
    except (OSError, ValueError) as error:
        (directory / 'pixel-error').write_text(str(error))
        raise
    finally:
        try:
            sys.stdout.buffer.write(b'\x1b[0m\x1b[?25h\x1b[H\x1b[2J')
            sys.stdout.buffer.flush()
        finally:
            termios.tcsetattr(0, termios.TCSANOW, settings)


def binding(library, name, return_type, *arguments):
    method = getattr(library, name)
    method.restype = return_type
    method.argtypes = arguments
    return method


def check_witness(directory):
    error = directory / 'pixel-error'
    if error.is_file():
        raise RuntimeError(error.read_text())


def initial_coordinate(directory, samples):
    from PIL import Image

    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        check_witness(directory)
        if (ready := directory / 'pixel-ready').is_file():
            state = json.loads(ready.read_text())
            if state != dict(grid=list(GRID), samples=samples):
                raise ValueError(f'witness settings disagree with the observer: {state}')
            data = subprocess.check_output(['import', '-window', 'root', 'png:-'], timeout=5)
            with Image.open(BytesIO(data)) as frame:
                screenshot = frame.convert('RGB')
            matches = [(x, y) for y in range(0, screenshot.height, 4) for x in range(0, screenshot.width, 4)
                       if screenshot.getpixel((x, y)) == COLORS[0]]
            if len(matches) >= 24:
                _, y = matches[len(matches) // 2]
                interior = [x for x, point_y in matches if point_y == y]
                return interior[len(interior) // 2], y
        time.sleep(0.1)
    raise TimeoutError('initial terminal color did not reach the X11 root window')


def measure_pixels(directory, samples, window, pid):
    owner = subprocess.check_output(['xprop', '-id', str(window), '_NET_WM_PID'], timeout=5, text=True)
    if not re.fullmatch(rf'_NET_WM_PID\(CARDINAL\) = {pid}\s*', owner):
        raise ValueError('the X11 window does not identify the expected desktop process')
    if (directory / 'pixel-consumed').exists() or (directory / 'pixel-complete').exists():
        raise ValueError('use a fresh witness directory for each run')

    x11 = CDLL('libX11.so.6')
    xtest = CDLL('libXtst.so.6')
    display = binding(x11, 'XOpenDisplay', c_void_p, c_void_p)(None)
    if not display:
        raise RuntimeError('could not open the X11 display')
    close = binding(x11, 'XCloseDisplay', c_int, c_void_p)
    try:
        root = binding(x11, 'XDefaultRootWindow', c_ulong, c_void_p)(display)
        image = binding(x11, 'XGetImage', c_void_p, c_void_p, c_ulong, c_int, c_int,
                        c_uint, c_uint, c_ulong, c_int)
        read_pixel = binding(x11, 'XGetPixel', c_ulong, c_void_p, c_int, c_int)
        destroy = binding(x11, 'XDestroyImage', c_int, c_void_p)
        focus = binding(x11, 'XGetInputFocus', c_int, c_void_p, POINTER(c_ulong), POINTER(c_int))
        keycode = binding(x11, 'XKeysymToKeycode', c_ubyte, c_void_p, c_ulong)(display, ord('x'))
        if not keycode:
            raise RuntimeError('the X11 keyboard lacks the x key')
        sync = binding(x11, 'XSync', c_int, c_void_p, c_int)
        inject = binding(xtest, 'XTestFakeKeyEvent', c_int, c_void_p, c_uint, c_int, c_ulong)
        x, y = initial_coordinate(directory, samples)
        active = c_ulong()
        revert = c_int()

        def read():
            frame = image(display, root, x, y, 1, 1, c_ulong(-1), 2)
            if not frame:
                raise RuntimeError('could not read the visible pixel')
            try:
                return read_pixel(frame, 0, 0) & 0xffffff
            finally:
                destroy(frame)

        rgb = [red << 16 | green << 8 | blue for red, green, blue in COLORS]
        if read() != rgb[0]:
            raise RuntimeError('the witness moved before measurement')
        elapsed_ns = []
        for index in range(samples + WARMUP):
            expected = rgb[(index + 1) % 2]
            sync(display, 0)
            focus(display, byref(active), byref(revert))
            if active.value != window:
                raise RuntimeError('X11 focus moved away from the selected terminal window')
            began = time.perf_counter_ns()
            if not inject(display, keycode, True, 0) or not inject(display, keycode, False, 0):
                raise RuntimeError('XTEST key injection was refused')
            sync(display, 0)
            while True:
                if read() == expected:
                    if index >= WARMUP:
                        elapsed_ns.append(time.perf_counter_ns() - began)
                    break
                if time.perf_counter_ns() - began > 2_000_000_000:
                    check_witness(directory)
                    raise TimeoutError(f'pixel did not change at sample {index}')
                time.sleep(0.0005)
            time.sleep(0.02)
        if not (directory / 'pixel-consumed').is_file():
            check_witness(directory)
            raise RuntimeError('the pixel changed without a complete PTY key receipt')
        (directory / 'pixel-complete').touch()
        ordered = sorted(elapsed_ns)
        return dict(actual_grid=list(GRID), window=window, witness_xy=[x, y], colors=COLORS,
                    elapsed_ns=elapsed_ns, excluded_warmup=WARMUP,
                    median_ms=statistics.median(elapsed_ns) / 1e6,
                    p95_ms=ordered[(95 * samples + 99) // 100 - 1] / 1e6,
                    p99_ms=ordered[(99 * samples + 99) // 100 - 1] / 1e6)
    finally:
        close(display)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_subparsers(dest='mode', required=True)
    witness = modes.add_parser('witness')
    measure = modes.add_parser('measure')
    for command in (witness, measure):
        command.add_argument('directory', type=Path)
        command.add_argument('--samples', type=int, default=100)
    measure.add_argument('--window-id', required=True, type=lambda value: int(value, 0))
    measure.add_argument('--host-pid', required=True, type=int)
    measure.add_argument('--binary-sha256', required=True)
    measure.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    if not 1 <= args.samples <= 10000:
        parser.error('samples must be 1..10000')
    if args.mode == 'witness':
        inside_terminal(args.directory, args.samples)
        return 0
    if not re.fullmatch('[0-9a-f]{64}', args.binary_sha256):
        parser.error('binary SHA-256 must be 64 lowercase hex characters')

    process_identity = runpy.run_path(str(Path(__file__).with_name('terminal-baseline.py')))['process_identity']
    with args.output.open('x') as output:
        report = dict(schema=1, scope='XTEST key through app PTY to visible XGetImage pixel',
                      display=os.environ.get('DISPLAY'), kernel=platform.release(),
                      utc=time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
                      script_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                      expected_executable_sha256=args.binary_sha256, load_before=list(os.getloadavg()))
        try:
            identity = process_identity(args.host_pid)
            report['host'] = identity
            if identity['executable_sha256'] != args.binary_sha256:
                raise ValueError('the desktop process executable does not match the supplied SHA-256')
            measured = measure_pixels(args.directory, args.samples, args.window_id, args.host_pid)
            if identity != process_identity(args.host_pid):
                raise RuntimeError('desktop process identity changed')
            report.update(measured, status='ok')
        except (Exception, KeyboardInterrupt) as error:
            report.update(status='failed', error=f'{type(error).__name__}: {error}')
        report['load_after'] = list(os.getloadavg())
        output.write(json.dumps(report, indent=2) + '\n')
    return int(report['status'] != 'ok')


if __name__ == '__main__':
    sys.exit(main())
