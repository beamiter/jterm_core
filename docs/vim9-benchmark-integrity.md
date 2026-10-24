# Native Vim benchmark sample integrity

The next buffer-performance work needs trustworthy samples before optimization.
`benchmark_render.py` measures PTY Ctrl-E to a semantic full frame;
`benchmark_scroll.py` measures a source-bound scroll request through its ordered
semantic acknowledgement and response. Neither measures displayed frame time,
input-to-photon latency, GPU utilization or GUI FPS.

## Accepted samples

A newer render revision alone is not evidence that a scroll completed. Both
benchmarks now require a supported frame with the same active window, buffer,
changed tick, viewport geometry and horizontal offset. The active source
viewport must advance by exactly one line; every sibling must retain its source
identity and viewport. Closing a split, editing the buffer, changing layout,
replacing a window or receiving an unrelated redraw cannot count as a fast
scroll sample. A sample that never receives the correct frame fails by timeout;
it is not silently dropped from the distribution.

The source-bound benchmark additionally requires the request's scroll ACK on
the active window, not an arbitrary sibling. Its existing requirement that the
semantic acknowledgement precede the successful response remains in place.
The legacy benchmark still uses full frames, keeping source extraction cost
separate from delta transport savings. Cursor movement and Unicode paint runs
are not mistaken for source-generation changes.

`benchmark_metrics.py` keeps these decisions independent of Vim and GUI code.
Its regression suite exercises stale/unrelated frames, wrong-split ACKs, changed
text, topology and viewport identity, single/split layouts and invalid workloads.
The real-Vim smoke gate complements those synthetic cases; it does not replace
frontend pixel, input-method, compositor or hardware acceptance tests.

## Statistics and reproducibility

P95 uses the nearest-rank convention, `ceil(0.95 * N)` with one-based ranks.
For 20 observations this is the 19th; for 100 it is the 95th. Empty, negative
and non-finite latency samples are rejected. Preserve sample count, percentile
method, Vim version, window count, workload, runtime revision and machine
conditions when comparing results. Old reports used a different percentile
index; remeasure rather than treating that difference as a speed improvement.

Both APIs and command-line entrypoints reserve at least `samples + 200` source
lines. This avoids counting a no-op at EOF as a completed scroll. The workload
supports one or two splits, with wrapping disabled by the real editor harness.

Run from the repository root:

```sh
python3 runtime/vim9/test_benchmark_metrics.py -v
python3 runtime/vim9/benchmark_render.py --lines 100000 --samples 100 --windows 1 2
python3 runtime/vim9/benchmark_scroll.py --lines 100000 --samples 160 --windows 1 2
```

Both benchmarks retain `--runtime /path/to/bridge.vim` for controlled runtime
comparisons. The CI smoke job deliberately uses only five scrolls per workload
and no wall-time threshold: its purpose is sample correctness on shared
runners, not a stable performance estimate or a latency improvement claim.

This change touches measurement, tests and CI only. Vim editing behavior, the
wire protocol, frontend renderers and their existing shared-core bundles are
unchanged. The next optimization should be justified by paired measurements of
the same workload, followed by real GUI scroll, click and selection validation.
