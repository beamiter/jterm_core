# Five-host terminal roundtrip baseline

This Linux standard-library probe runs **inside an actual Aether, Ember, Frost,
Forge or Anvil terminal**. It emits a fixed public scrolling log workload (ASCII, SGR color,
UTF-8) and requests cursor position after each batch. A DSR response is a
PTY/parser processing barrier, **not a GPU-present fence**. This is not a parser-only
microbenchmark, but is also not a complete application benchmark or a Vim workload.

The PTY test responder validates the measuring harness. Native product runs
provide the roundtrip performance results.

## Run protocol

Use the same otherwise-idle Linux host, kernel/boot, display backend, monitor,
window grid (e.g. 120x40), scale, font, history limit, release build profile and
shell integration setting. Start a fresh, single-window process per sample run;
close other terminal workloads. Do not compile while collecting. Record these
settings, CPU model/governor, system RAM and display/compositor versions with the
results. Use the exact executable built from the declared full commit, verify that
build separately, and pass its actual process PID (not the shell or probe PID).
The executable digest and PID start time are recorded; the declared revision is
operator metadata, **not automatically proven source provenance**. Pass the
expected digest with `--binary-sha256` to check the running executable before
timing. For a candidate built from a working tree, supply that digest and omit
`--revision`. The default `--rss-scope single-process` excludes renderer/helper RSS.
Use `--rss-scope process-tree` for the explicitly limited descendant accounting
below; neither mode measures total physical memory or a high-water mark.

Within that terminal, run (replace all example values):

```sh
python3 terminal-baseline.py --host ember --host-pid 12345 \
  --revision FULL_40_HEX_COMMIT --samples 100 --output ember-round1.json
```

Repeat for all five hosts in rotating order for at least ten rounds, with each
host starting the round twice. Retain failed runs;
never count a timeout as zero latency or rerun until lucky. Do not type, paste or
resize during measurement. An unsupported DSR is a blocked measurement, not poor
performance. Probe timeout includes output-write backpressure; terminal settings
are restored on Python-handled errors and KeyboardInterrupt. SIGTERM/SIGKILL do
not run that cleanup; use `stty sane` in the affected terminal if necessary. Failed
reports retain status/error, not partial timing samples or a successful summary.
The probe does not enter alternate screen: it deliberately exercises scrolling
and bounded history. It visibly fills the terminal and changes cursor position.

## Metrics and noise

* One warm-up batch is excluded; 100 recorded batches follow.
* Each latency encloses the complete output write and matching DSR read. P95 and P99
  use nearest rank, preserving every raw sample. It is **batch barrier
  latency**, not keystroke latency, frame latency, startup, or a responsiveness SLA.
* Throughput is sum of public payload bytes / sum of measured intervals, MiB/s.
  DSR query/response bytes are excluded from the numerator, included in elapsed
  time. Backpressure, terminal processing, scheduling and PTY roundtrip are included.
* `host_cpu_ms` measures user and system CPU time for the desktop host process
  from the end of warm-up through the last measurement boundary. It includes
  work between batches and excludes other processes, including WebKit helpers.
* RSS is sampled immediately before/after each batch from the actual host PID.
  `rss_sampled_max_kib` is **not peak RSS**, allocation accounting, or a hard memory
  ceiling. Transient peaks and helper processes are missed. Sampling overhead is
  outside each timed interval, but perturbs inter-batch pacing. Executable hashing
  is outside timing and can warm file cache.
* Load averages and boot ID help identify incompatible runs, not eliminate noise.
  Report per-round medians, p95 and ranges; don't pool away between-round variation.
  Compare only matched configuration/revision/binary/source identities. Shared-cloud
  CPU contention and DVFS remain possible. No absolute performance CI gate yet.

## X11 key-to-pixel companion probe

`terminal-key-to-pixel.py` measures XTEST key injection through the application's
PTY to a visible color change read with `XGetImage`. Run it in a separate Xvfb
display with a fresh instance of the application. It requires Python Pillow,
ImageMagick `import`, `xprop`, `libX11` and `libXtst`. Keep the same grid, screen,
scale, software renderer, terminal profile and font for each baseline and
candidate. The grid is fixed at 120 columns by 40 rows.

Create a private directory with `mktemp -d` and run the witness in the terminal
being measured. Enter the directory's absolute path and leave the terminal focused.

```sh
python3 terminal-key-to-pixel.py witness /path/to/witness-dir --samples 100
```

From the process driving the same X11 display, run the observer. Supply the
actual focused top-level window ID, its desktop process PID and the SHA-256 of
the executable under test.

```sh
python3 terminal-key-to-pixel.py measure /path/to/witness-dir --samples 100 \
  --window-id 0x400001 --host-pid 12345 --binary-sha256 EXPECTED_64_HEX_DIGEST \
  --output key-to-pixel-round1.json
```

The witness acknowledges each exact input byte by alternating two background
colors, checks the grid at both ends, and records successful consumption of the
entire input sequence. The observer checks process identity and focus, excludes
five warm-up inputs, and retains all intervals and P50, P95 and P99. The intervals
include X11 readback sampling and are scoped to Xvfb. The fixed 60 Hz device,
compositor presentation timing, real Vim workloads, process-tree high-water
memory and sustained flood response require separate measurements.

## Optional descendant RSS census (schema 2)

Pass `--rss-scope process-tree` to sample the host and its current descendants
before/after each DSR batch. The default remains `single-process`. Each successful
sample retains PID, parent PID, start ticks and RSS for every selected process,
alongside the sum. The measurement process and its descendants are excluded; its
shell and other host descendants are included. Compare only the same scope and
matching shell/workload setup. The host PID must not be this measuring process.

This is **summed RSS**, not distinct physical memory: shared pages can be counted
  in multiple processes. It does not include a helper launched by a pre-existing
external service or a daemon reparented away from the host, and cannot observe
short-lived children entirely between scans. It is not peak RSS, process-tree
high-water memory, PSS, or proof of a resource ceiling.

The census checks parent/start identity around each RSS read, then rescans tree
membership. Observed PID reuse, reparenting, child birth/exit during a census,
missing RSS and access errors fail the entire run rather than fabricate zero or
publish a partial success. `/proc` must expose the same host PID namespace and
readable process metadata; hidden processes can invalidate the declared scope.
These checks detect observable races, not an atomic OS snapshot. A new child
between completed samples is allowed and recorded in the next sample. Persistent
process churn can make this conservative mode unusable; retain the failure rather
than silently fall back to a smaller memory scope.

The census is outside timed intervals but can perturb pacing; it scans `/proc`
twice per sample boundary and has safety caps of 65,536 census records and 4,096
selected processes. Keep raw membership in reports and report these limits.

## Harness verification

```sh
python3 -m unittest discover -s . -p 'test_terminal_baseline.py' -v
```

Tests cover descendant selection, real parent/child sampling, PID reuse, census
churn, missing RSS, full payload/backpressure, fragmented replies, timeout, extraneous
input, terminal restoration, PID reuse, RSS and metric sample plumbing. They install
nothing and do not launch or impersonate any of the five terminal applications.

## Shared engine frame publication

```sh
cargo run --locked --release --manifest-path crates/jterm_engine/Cargo.toml \
  --example frame_publication_baseline > frame-publication.csv
```

This component probe records separate feed, capture and delta timings for
query-only input, cursor movement and a single edited row. It warms up each
workload and then records 400 observations at 120×40 and 256×128, with styled
rows and grapheme annotations already present. For revision comparisons use
the same example source, CPU, Rust toolchain and release profile on both
source revisions. The CSV records a skipped delta as `changed_rows=-1`.
