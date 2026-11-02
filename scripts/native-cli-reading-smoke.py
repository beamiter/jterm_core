#!/usr/bin/env python3
"""Exercise active CLI reading in real frontends on a private Xvfb display.

No provider or network is involved: a raw-TTY codex fixture renders uniquely
numbered, machine-readable color bars and records input. Evidence comes from
displayed pixels, not the frontend's own claims about its scroll position.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import time

import numpy as np
from PIL import ImageGrab


COLORS = [(241, 88, 200), (240, 190, 40), (50, 200, 230)]
FIXTURE = r'''#!PYTHON
import json, os, select, sys, termios, tty
from pathlib import Path
root = Path(os.environ['CLI_READING_PROBE'])
fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
fifo = os.open(root / 'control', os.O_RDWR | os.O_NONBLOCK)
next_row = 0
def rows(count):
    global next_row
    out = ['\r\033[2K']
    for number in range(next_row, next_row + count):
        out.append('\033[38;2;241;88;200m▀▀')
        for shift in reversed(range(10)):
            color = '50;200;230' if number & (1 << shift) else '240;190;40'
            out.append('\033[38;2;' + color + 'm▀')
        out.append('\033[38;2;241;88;200m▀▀\033[0m ROW %04d continuous session history\r\n' % number)
    next_row += count
    out.append('codex> reading fixture (q exits)')
    sys.stdout.write(''.join(out))
    sys.stdout.flush()
    (root / 'state.json').write_text(json.dumps({'rows': next_row, 'argv': sys.argv,
        'claude_classic': os.environ.get('CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN')}))
try:
    tty.setraw(fd)
    rows(220)
    pending = b''
    while True:
        ready, _, _ = select.select([fd, fifo], [], [], 1)
        if fd in ready:
            data = os.read(fd, 4096)
            with (root / 'received.bin').open('ab') as log:
                log.write(data)
            if b'q' in data:
                break
        if fifo in ready:
            pending += os.read(fifo, 4096)
            while b'\n' in pending:
                command, pending = pending.split(b'\n', 1)
                if command.startswith(b'append '):
                    rows(int(command.split()[1]))
finally:
    termios.tcsetattr(fd, termios.TCSANOW, old)
    os.close(fifo)
    print('\r\nfixture finished', flush=True)
'''

PROFILE = r'''export HISTFILE=/dev/null
set -o history
unset HISTCONTROL HISTIGNORE HISTTIMEFORMAT
__probe_ready=0
__probe_running=0
__probe_sequence=0
__probe_start() {
    [[ $__probe_ready == 1 && $BASH_COMMAND != __probe_prompt ]] || return
    __probe_ready=0
    __probe_running=1
    ((__probe_sequence++)) || :
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
    printf '\e]133;C;id=reading-%s;cmdline_url=%s\a' "$__probe_sequence" "$encoded"
}
__probe_prompt() {
    local status=$?
    if [[ $__probe_running == 1 ]]; then
        printf '\e]133;D;%s;id=reading-%s\a' "$status" "$__probe_sequence"
    fi
    __probe_running=0
    printf '\e]133;A\a'
    __probe_ready=1
    printf ready > "$CLI_READING_PROBE/shell-ready"
}
PROMPT_COMMAND=__probe_prompt
PS0=''
PS1='probe> \[\e]133;B\a\]'
trap '__probe_start' DEBUG
'''


def visible_rows(frame):
    """Decode 10-bit row IDs and their actual screen positions from color bars."""
    # Sample just the fixture's bar columns. Full-window color arithmetic can
    # consume most of a frame on software-rendered CI and miss real motion.
    crop_left = 330
    pixels = np.asarray(frame)[:, crop_left:600, :3].astype(np.int16)
    pink = np.max(np.abs(pixels - COLORS[0]), axis=2) < 10
    ys = np.flatnonzero(pink.sum(axis=1) > 18)
    groups = np.split(ys, np.flatnonzero(np.diff(ys) > 1) + 1)
    decoded = []
    for group in groups:
        if not len(group):
            continue
        y = int(group[len(group) // 2])
        xs = np.flatnonzero(pink[y])
        if len(xs) < 16:
            continue
        # Font rasterizers may leave a subpixel seam between block glyphs.
        # The two sentinel cells at either end establish all 14 cell centers.
        cell = (int(xs[-1]) + 1 - int(xs[0])) / 14
        x = int(xs[0])
        value = 0
        valid = True
        for bit in range(10):
            at = int(x + (2.5 + bit) * cell)
            if at >= pixels.shape[1]:
                valid = False
                break
            distances = [np.max(np.abs(pixels[y, at] - color)) for color in COLORS[1:]]
            if min(distances) > 10:
                valid = False
                break
            value = value * 2 + int(distances[1] < distances[0])
        if valid:
            decoded.append({'id': value, 'y': int(group[0]), 'height': len(group), 'x': x + crop_left,
                            'cell': cell})
    return decoded


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('app', choices=['anvil', 'forge', 'ember', 'frost'])
    parser.add_argument('mode', choices=['block', 'chat'])
    parser.add_argument('--provider', choices=['codex', 'claude'], default='codex')
    parser.add_argument('--direct', action='store_true',
                        help='launch exact CLI argv without an OSC-integrated shell')
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    os.umask(0o077)
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False, mode=0o700)
    task_home = out / 'home'
    task_home.mkdir(mode=0o700)
    runtime = out / 'runtime'
    runtime.mkdir(mode=0o700)
    (out / 'bin').mkdir()
    fixture = out / ('bin/' + args.provider)
    fixture.write_text(FIXTURE.replace('#!PYTHON', '#!' + sys.executable, 1))
    fixture.chmod(0o700)
    os.mkfifo(out / 'control')
    for name in ['.bashrc', '.bash_profile']:
        (task_home / name).write_text(PROFILE)
    env = os.environ.copy()
    for key in ['DISPLAY', 'WAYLAND_DISPLAY', 'SWAYSOCK', 'DBUS_SESSION_BUS_ADDRESS']:
        env.pop(key, None)
    env.update(HOME=str(task_home), XDG_CONFIG_HOME=str(task_home / '.config'),
               XDG_DATA_HOME=str(task_home / '.local/share'),
               XDG_STATE_HOME=str(task_home / '.local/state'),
               XDG_CACHE_HOME=str(task_home / '.cache'), XDG_RUNTIME_DIR=str(runtime),
               CLI_READING_PROBE=str(out), SHELL='/bin/bash', LANG='C.UTF-8',
               LIBGL_ALWAYS_SOFTWARE='1', GDK_BACKEND='x11', GSK_RENDERER='cairo',
               ICED_BACKEND='tiny-skia', RUST_LOG='warn')
    # Make discovery possible before the frontend prepares the child's PATH
    # shim. A shell profile must not accidentally prepend the fixture again
    # above that shim and bypass the integration being tested.
    env['PATH'] = str(out / 'bin') + os.pathsep + env.get('PATH', '')
    config_dir = Path(env['XDG_CONFIG_HOME']) / args.app
    config_dir.mkdir(parents=True)
    shell = str(fixture) if args.direct else '/bin/bash'
    config = (f'shell = {json.dumps(shell)}\nterminal_mode = "{args.mode}"\n'
              'opacity = 1.0\nascii_organism_enabled = false\n')
    if args.app in ['anvil', 'forge']:
        config += 'font = "DejaVu Sans Mono 13"\n'
    if args.app == 'ember':
        config += 'app_renderer = "glow"\n'
    if args.app in ['ember', 'frost']:
        config += 'scrollbar_visibility = "always"\n'
    (config_dir / 'config.toml').write_text(config)
    (config_dir / 'config.toml').chmod(0o600)
    result = {'app': args.app, 'mode': args.mode, 'provider': args.provider, 'direct': args.direct,
              'renderer': 'Xvfb/software',
              'binary_sha256': hashlib.sha256(args.binary.read_bytes()).hexdigest()}
    processes, logs = [], []

    def run(*command, check=True):
        return subprocess.run(command, env=env, capture_output=True, text=True,
                              timeout=10, check=check)

    def xdo(*command):
        return run('xdotool', *map(str, command))

    def capture(name=None):
        frame = ImageGrab.grab(xdisplay=env['DISPLAY'])
        if name:
            frame.save(out / (name + '.png'))
        return frame

    def wait_for(predicate, description, seconds=15):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            value = predicate()
            if value:
                return value
            time.sleep(0.05)
        raise AssertionError('timed out waiting for ' + description)

    def rows(name):
        decoded = visible_rows(capture(name))
        assert len(decoded) >= 4, f'{name}: fewer than four visible transcript rows'
        ids = [r['id'] for r in decoded]
        assert ids == list(range(ids[0], ids[0] + len(ids))), f'{name}: discontinuous rows: {ids}'
        result[name] = decoded
        return decoded

    def sample_motion(name, action):
        before = rows(name + '-before')
        action()
        start = time.monotonic()
        samples, frames = [], []
        for _ in range(30):
            frame = capture()
            decoded = visible_rows(frame)
            assert len(decoded) >= 4, name + ': blank intermediate frame'
            ids = [r['id'] for r in decoded]
            assert ids == list(range(ids[0], ids[0] + len(ids))), name + ': discontinuous intermediate frame'
            samples.append({'seconds': time.monotonic() - start, 'rows': decoded})
            frames.append(frame)
            time.sleep(1 / 90)
        frames[0].save(out / (name + '.gif'), save_all=True, append_images=frames[1:],
                       duration=18, loop=0)
        after = rows(name + '-after')
        signatures = {(s['rows'][0]['id'], s['rows'][0]['y'], s['rows'][0]['height']) for s in samples}
        result[name] = {'positions': len(signatures), 'samples': samples}
        assert before[0]['id'] != after[0]['id'], name + ': history did not move'
        return before, after

    def thumb(frame, document):
        pixels = np.asarray(frame)[:, :, :3].astype(np.int16)
        low = document[0]['y']
        row_height = document[1]['y'] - low
        high = min(730, document[-1]['y'] + 2 * row_height)
        strip = pixels[low:high, 1020:1095]
        minimum, maximum, spread = strip.min(2), strip.max(2), np.ptp(strip, axis=2)
        neutral = (((minimum > 65) & (maximum < 210) & (spread < 65))
                   | ((minimum < 180) & (maximum > 100) & (spread > 35)))
        candidates = []
        for x in range(neutral.shape[1]):
            ys = np.flatnonzero(neutral[:, x])
            for group in np.split(ys, np.flatnonzero(np.diff(ys) > 1) + 1):
                if 20 < len(group) < (high - low) * 0.6:
                    candidates.append((len(group), x + 1020, int(group[0]) + low,
                                       int(group[-1]) + low))
        assert candidates, 'visible draggable native session scrollbar'
        length, x, top, bottom = max(candidates)
        columns = [c[1] for c in candidates if c[0] >= length - 2 and abs(c[2] - top) <= 2]
        return int(np.median(columns)), (top + bottom) // 2

    def drag_scrollbar(name, target_y):
        before = rows(name + '-before')
        x, y = thumb(capture(), before)
        xdo('mousemove', x, y)
        time.sleep(0.15)
        # Give the host one real frame between press, movement and release.
        xdo('mousedown', 1)
        time.sleep(0.08)
        for i in range(1, 9):
            xdo('mousemove', x, round(y + (target_y - y) * i / 8))
            time.sleep(0.015)
        xdo('mouseup', 1)
        time.sleep(0.4)
        after = rows(name + '-after')
        result[name] = {'grab': [x, y], 'target_y': target_y}
        return before, after

    try:
        log = (out / 'xvfb.log').open('w')
        logs.append(log)
        server = subprocess.Popen(['Xvfb', '-displayfd', '1', '-screen', '0', '1280x900x24',
                                   '-nolisten', 'tcp'], stdout=subprocess.PIPE, stderr=log, text=True)
        processes.append(server)
        assert select.select([server.stdout], [], [], 10)[0], 'Xvfb readiness'
        number = server.stdout.readline().strip()
        assert number.isdigit(), 'Xvfb display number'
        env['DISPLAY'] = ':' + number
        log = (out / 'app.log').open('w')
        logs.append(log)
        app = subprocess.Popen([str(args.binary.resolve())], env=env, cwd=out, stdout=log, stderr=log)
        processes.append(app)

        def find_window():
            assert app.poll() is None, 'application exited; see app.log'
            found = run('xdotool', 'search', '--onlyvisible', '--pid', str(app.pid), check=False)
            return found.stdout.splitlines()[-1] if found.returncode == 0 and found.stdout.strip() else None

        window = wait_for(find_window, 'application window', 25)
        xdo('windowmove', window, 0, 0)
        xdo('windowsize', window, 1100, 760)
        xdo('windowfocus', '--sync', window)
        ready = 'state.json' if args.direct else 'shell-ready'
        wait_for(lambda: (out / ready).exists(), 'initial child readiness')
        time.sleep(0.7)
        if args.app in ['ember', 'frost']:
            # Startup may show the API-chat pane while the shell is ready in
            # the background. Select the first real terminal through its
            # public tab shortcut before exercising native CLI ownership.
            xdo('key', '--clearmodifiers', 'Escape', 'ctrl+1')
            time.sleep(0.5)
        if args.app == 'ember':
            # Ember keeps its API panel selected across session shortcuts.
            # In this empty fixed-size workspace, the first terminal is the
            # sidebar row below TERMINALS (no live conversations above it).
            xdo('mousemove', 85, 352)
            time.sleep(0.2)
            xdo('mousedown', 1)
            time.sleep(0.1)
            xdo('mouseup', 1)
            time.sleep(0.3)
        capture('startup')
        if not args.direct:
            xdo('type', '--clearmodifiers', '--delay', 30, '--', args.provider)
            xdo('key', '--clearmodifiers', 'Return')
        wait_for(lambda: (out / 'state.json').exists(), 'active raw-TTY CLI')
        launch = json.loads((out / 'state.json').read_text())
        if args.provider == 'codex':
            assert '--no-alt-screen' in launch['argv'], 'Codex launch retains main-screen history'
        else:
            assert launch['claude_classic'] == '1', 'Claude launch selects classic main-screen renderer'
        time.sleep(1)
        initial = rows('initial')
        assert initial[-1]['id'] == 219, 'starts at live edge'
        before, after = sample_motion('page-up', lambda: xdo('key', '--clearmodifiers', 'Prior'))
        assert {r['id'] for r in before} & {r['id'] for r in after}, 'page-up needs reading overlap'
        read_anchor = after
        with (out / 'control').open('w') as control:
            control.write('append 24\n')
        wait_for(lambda: json.loads((out / 'state.json').read_text())['rows'] == 244, 'streamed output')
        time.sleep(0.6)
        appended = rows('streaming-history')
        anchor = next(row for row in read_anchor if row['id'] == read_anchor[len(read_anchor) // 2]['id'])
        moved = next((row for row in appended if row['id'] == anchor['id']), None)
        assert moved and abs(moved['y'] - anchor['y']) <= 1, 'new output moved the reading anchor'
        pointer = appended[len(appended) // 2]
        xdo('mousemove', pointer['x'] + int(pointer['cell'] * 7), pointer['y'] + 5)
        sample_motion('wheel-up', lambda: xdo('mousedown', 4, 'mouseup', 4))
        assert result['wheel-up']['positions'] >= 4, 'wheel must paint intermediate pixel positions'
        sample_motion('page-down', lambda: xdo('key', '--clearmodifiers', 'Next'))
        current = rows('before-drag')
        _, thumb_y = thumb(capture(), current)
        before, after = drag_scrollbar('drag-history', max(current[0]['y'] + 40, thumb_y - 100))
        assert after[0]['id'] < before[0]['id'], 'dragging scrollbar upward moved history'
        # The fixed-size fixture's transcript rail ends above the dock at
        # about y=704. A directly launched child can leave unused blank rows
        # after an initial resize; its last glyph is not the rail's bottom.
        _, latest = drag_scrollbar('drag-latest', 700)
        assert latest[-1]['id'] == 243, 'dragging scrollbar to bottom reveals latest output'
        received = (out / 'received.bin').read_bytes() if (out / 'received.bin').exists() else b''
        result['received_hex'] = received.hex()
        assert b'\x1b[5' not in received and b'\x1b[6' not in received, 'local paging leaked into CLI input'
        result['status'] = 'pass'
    except Exception as error:
        result.update(status='fail', error=str(error))
        try:
            capture('failure')
        except Exception:
            pass
        raise
    finally:
        for process in reversed(processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        for log in logs:
            log.close()
        (out / 'result.json').write_text(json.dumps(result, indent=2))
        print(json.dumps({key: result[key] for key in ['app', 'mode', 'status', 'error'] if key in result}))


if __name__ == '__main__':
    main()
