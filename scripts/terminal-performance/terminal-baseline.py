#!/usr/bin/env python3
"""Linux in-terminal PTY/parser roundtrip probe. NOT input-to-pixel latency."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import select
import statistics
import sys
import termios
import time
import tty

REPLY = re.compile(rb'\x1b\[[1-9][0-9]*;[1-9][0-9]*R')
WORKLOAD = (b'public-baseline 0123456789 abcdefghijklmnopqrstuvwxyz '
            b'\x1b[31mred\x1b[0m ' + '中文 é λ'.encode() + b'\r\n') * 512


def percentile95(values):
    return sorted(values)[math.ceil(len(values) * .95) - 1]


def process_identity(pid):
    stat = Path(f'/proc/{pid}/stat').read_text()
    # comm may contain spaces and parentheses; fields after last ')' start at 3.
    starttime = stat[stat.rfind(')') + 2:].split()[19]
    exe = Path(f'/proc/{pid}/exe')
    digest = hashlib.sha256()
    with exe.open('rb') as binary:
        for chunk in iter(lambda: binary.read(1024 * 1024), b''):
            digest.update(chunk)
    return dict(pid=pid, start_ticks=starttime,
                executable_sha256=digest.hexdigest())


def rss_kib(pid, identity):
    stat = Path(f'/proc/{pid}/stat').read_text()
    if stat[stat.rfind(')') + 2:].split()[19] != identity['start_ticks']:
        raise RuntimeError('host PID was reused')
    status = Path(f'/proc/{pid}/status').read_text()
    match = re.search(r'^VmRSS:\s+(\d+) kB$', status, re.M)
    if not match:
        raise RuntimeError('host RSS unavailable; no fabricated zero')
    return int(match[1])


def exchange(fd, payload, timeout):
    """Bound all writes and reads by one deadline; reject extraneous input."""
    end = time.monotonic() + timeout
    remaining = memoryview(payload + b'\x1b[6n')
    reply = bytearray()
    while remaining or not REPLY.fullmatch(reply):
        left = end - time.monotonic()
        if left <= 0:
            raise TimeoutError('DSR barrier timed out (not a successful sample)')
        rd, wr, _ = select.select([fd], [fd] if remaining else [], [], left)
        if rd and remaining:
            raise RuntimeError('input or response arrived before query write completed')
        if wr:
            try:
                count = os.write(fd, remaining)
                remaining = remaining[count:]
            except BlockingIOError:
                pass
        if rd:
            try:
                data = os.read(fd, 256)
            except BlockingIOError:
                continue
            if not data:
                raise RuntimeError('terminal closed')
            reply.extend(data)
            if len(reply) > 64 or not b'\x1b['.startswith(reply[:2]):
                raise RuntimeError('unexpected input during isolated measurement')
    # One outstanding query at a time. No response is silently reused.
    if select.select([fd], [], [], 0)[0]:
        raise RuntimeError('extra input after DSR reply')


def measure(fd, pid, samples, timeout):
    identity = process_identity(pid)
    size = os.get_terminal_size(fd)
    old = termios.tcgetattr(fd)
    blocking = os.get_blocking(fd)
    raw = []
    try:
        tty.setraw(fd, when=termios.TCSANOW)
        os.set_blocking(fd, False)
        if select.select([fd], [], [], 0)[0]:
            raise RuntimeError('pending input; rerun without typing')
        exchange(fd, WORKLOAD, timeout)  # explicit excluded warm-up
        for i in range(samples):
            if os.get_terminal_size(fd) != size:
                raise RuntimeError('terminal resized during measurement')
            before = rss_kib(pid, identity)
            started = time.perf_counter_ns()
            exchange(fd, WORKLOAD, timeout)
            elapsed = time.perf_counter_ns() - started
            if os.get_terminal_size(fd) != size:
                raise RuntimeError('terminal resized during measurement')
            after = rss_kib(pid, identity)
            raw.append(dict(sample=i, bytes=len(WORKLOAD), elapsed_ns=elapsed,
                            rss_before_kib=before, rss_after_kib=after))
        if process_identity(pid) != identity:
            raise RuntimeError('host executable identity changed')
    finally:
        termios.tcsetattr(fd, termios.TCSANOW, old)
        os.set_blocking(fd, blocking)
    latencies = [s['elapsed_ns'] / 1e6 for s in raw]
    return dict(host=identity, columns=size.columns, rows=size.lines,
                workload_sha256=hashlib.sha256(WORKLOAD).hexdigest(), samples=raw,
                barrier_p95_ms=percentile95(latencies),
                barrier_median_ms=statistics.median(latencies),
                throughput_mib_s=sum(s['bytes'] for s in raw) / (1024**2) /
                    (sum(s['elapsed_ns'] for s in raw) / 1e9),
                rss_sampled_max_kib=max(max(s['rss_before_kib'], s['rss_after_kib']) for s in raw))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--host', required=True, choices=['ember', 'frost', 'forge', 'anvil'])
    parser.add_argument('--revision', required=True)
    parser.add_argument('--host-pid', required=True, type=int)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--samples', type=int, default=100)
    parser.add_argument('--timeout', type=float, default=10)
    args = parser.parse_args()
    if not re.fullmatch('[0-9a-f]{40}', args.revision):
        parser.error('revision must be full 40-character commit SHA')
    if not 20 <= args.samples <= 10000 or not 0 < args.timeout <= 120:
        parser.error('samples must be 20..10000; timeout must be (0,120]')
    # Never clobber an earlier report, even on a failed attempt.
    with args.output.open('x') as output:
        report = dict(schema=1, host_label=args.host, declared_revision=args.revision,
                      utc=time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
                      kernel=platform.release(), machine=platform.machine(),
                      boot_id=Path('/proc/sys/kernel/random/boot_id').read_text().strip(),
                      load_before=list(os.getloadavg()), python=platform.python_version(),
                      script_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                      scope='PTY/parser DSR barrier; no pixel or keyboard latency; single host PID sampled RSS')
        try:
            with open('/dev/tty', 'rb+', buffering=0) as terminal:
                report.update(measure(terminal.fileno(), args.host_pid, args.samples, args.timeout))
            report['status'] = 'ok'
        except (Exception, KeyboardInterrupt) as error:
            report.update(status='failed', error=f'{type(error).__name__}: {error}')
        report['load_after'] = list(os.getloadavg())
        json.dump(report, output, indent=2)
        output.write('\n')
    return 0 if report['status'] == 'ok' else 1


if __name__ == '__main__':
    sys.exit(main())
