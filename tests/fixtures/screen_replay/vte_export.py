#!/usr/bin/env python3
"""Feed raw PTY byte streams to a real libvte and print VTE's own text export.

This is the reference side of `screen_replay`'s differential test
(`screen_replay::tests::vte_differential_matches_real_vte`, run through a
headless display). It feeds the same bytes the replay sees into a
`Vte.Terminal` of the same geometry, with unlimited scrollback, waits until VTE
has processed its queue, and writes `Terminal.write_contents_sync` (history +
screen, soft-wrapped rows joined) unmodified. Normalisation is the Rust side's
job.

usage: vte_export.py FILE COLS ROWS [OFFSET:COLS:ROWS ...]
       vte_export.py --batch MANIFEST

Each OFFSET:COLS:ROWS resizes the terminal after the first OFFSET bytes, the
way the child's winsize changed while the capture ran. In batch mode every
MANIFEST line is `FILE COLS ROWS [OFFSET:COLS:ROWS ...]` and the output is, per
line in order, the export's byte length in decimal, a newline, then the
export. Cases without resizes share one settle wait, which keeps a few dozen
cases to about a second.
"""
import sys

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Vte", "3.91")
from gi.repository import Gio, GLib, Gtk, Vte  # noqa: E402

CHANGED = [0]


def settle():
    # Terminal.feed only queues bytes; VTE parses them from its own timer
    # (10 Hz while unmapped). Wait until no terminal has changed for a while.
    ctx = GLib.MainContext.default()
    quiet_us = 300_000
    last = (CHANGED[0], GLib.get_monotonic_time())
    while True:
        ctx.iteration(False)
        now = GLib.get_monotonic_time()
        if CHANGED[0] != last[0]:
            last = (CHANGED[0], now)
        elif now - last[1] > quiet_us:
            return


def parse_case(fields):
    path, cols, rows = fields[0], int(fields[1]), int(fields[2])
    resizes = []
    for spec in fields[3:]:
        off, c, r = (int(x) for x in spec.split(":"))
        resizes.append((off, c, r))
    return path, cols, rows, sorted(resizes)


def new_terminal(cols, rows):
    term = Vte.Terminal()
    term.set_scrollback_lines(-1)
    term.set_size(cols, rows)
    term.connect("contents-changed", lambda *_: CHANGED.__setitem__(0, CHANGED[0] + 1))
    return term


def export(term):
    out = Gio.MemoryOutputStream.new_resizable()
    term.write_contents_sync(out, Vte.WriteFlags.DEFAULT, None)
    out.close(None)
    return bytes(out.steal_as_bytes().get_data())


def run_with_resizes(path, cols, rows, resizes):
    data = open(path, "rb").read()
    term = new_terminal(cols, rows)
    start = 0
    for off, c, r in resizes:
        term.feed(data[start:off])
        settle()
        term.set_size(c, r)
        settle()
        start = off
    term.feed(data[start:])
    settle()
    return export(term)


def main():
    Gtk.init()
    if sys.argv[1] == "--batch":
        cases = [parse_case(line.split()) for line in open(sys.argv[2]) if line.strip()]
        results = [None] * len(cases)
        plain = []
        for index, (path, cols, rows, resizes) in enumerate(cases):
            if resizes:
                results[index] = run_with_resizes(path, cols, rows, resizes)
            else:
                term = new_terminal(cols, rows)
                term.feed(open(path, "rb").read())
                plain.append((index, term))
        settle()
        for index, term in plain:
            results[index] = export(term)
        for result in results:
            sys.stdout.buffer.write(b"%d\n" % len(result))
            sys.stdout.buffer.write(result)
    else:
        path, cols, rows, resizes = parse_case(sys.argv[1:])
        sys.stdout.buffer.write(run_with_resizes(path, cols, rows, resizes))


if __name__ == "__main__":
    main()
