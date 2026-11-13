# Four-host same-machine baseline: first measurable slice

This Linux standard-library probe runs **inside an actual Ember, Frost, Forge or
Anvil terminal**. It emits a fixed public scrolling log workload (ASCII, SGR color,
UTF-8) and requests cursor position after each batch. A DSR response is a
PTY/parser processing barrier, **not a GPU-present fence**. This is not a parser-only
microbenchmark, but is also not a complete application benchmark or a Vim workload.

No four-terminal measurement has been performed in this checkout. The PTY test
responder only validates the measuring harness and must never be reported as a
terminal performance result. Existing CI pixel correctness does not fill this gap.

## Run protocol

Use the same otherwise-idle Linux host, kernel/boot, display backend, monitor,
window grid (e.g. 120x40), scale, font, history limit, release build profile and
shell integration setting. Start a fresh, single-window process per sample run;
close other terminal workloads. Do not compile while collecting. Record these
settings, CPU model/governor, system RAM and display/compositor versions with the
results. Use the exact executable built from the declared full commit, verify that
build separately, and pass its actual process PID (not the shell or probe PID).
The executable digest and PID start time are recorded; the declared revision is
operator metadata, **not automatically proven source provenance**. The default `--rss-scope single-process` excludes renderer/helper RSS.
Use `--rss-scope process-tree` for the explicitly limited descendant accounting
below; neither mode measures total physical memory or a high-water mark.

Within that terminal, run (replace all example values):

```sh
python3 terminal-baseline.py --host ember --host-pid 12345 \
  --revision FULL_40_HEX_COMMIT --samples 100 --output ember-round1.json
```

Repeat for all four hosts in balanced rotating order, at least seven rounds:
Ember/Frost/Forge/Anvil, Frost/Forge/Anvil/Ember, and so on. Retain failed runs;
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
* Each latency encloses the complete output write and matching DSR read. p95 uses
  nearest rank (`ceil(.95*N)`), preserving every raw sample. It is **batch barrier
  latency**, not keystroke latency, frame latency, startup, or a responsiveness SLA.
* Throughput is sum of public payload bytes / sum of measured intervals, MiB/s.
  DSR query/response bytes are excluded from the numerator, included in elapsed
  time. Backpressure, terminal processing, scheduling and PTY roundtrip are included.
* RSS is sampled immediately before/after each batch from the actual host PID.
  `rss_sampled_max_kib` is **not peak RSS**, allocation accounting, or a hard memory
  ceiling. Transient peaks and helper processes are missed. Sampling overhead is
  outside each timed interval, but perturbs inter-batch pacing. Executable hashing
  is outside timing and can warm file cache.
* Load averages and boot ID help identify incompatible runs, not eliminate noise.
  Report per-round medians, p95 and ranges; don't pool away between-round variation.
  Compare only matched configuration/revision/binary/source identities. Shared-cloud
  CPU contention and DVFS remain possible. No absolute performance CI gate yet.

Remaining coverage: instrument actual keyboard injection to compositor-present
latency, same-machine real Vim workloads, process-tree high-water memory, resize/
reflow workloads and sustained natural flood response. Do not replace these with
controlled-stall correctness tests or this DSR proxy.

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
high-water memory, PSS, or proof of a resource ceiling. No four-host measurements
have been made by adding this option.

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
nothing and do not launch or impersonate any of the four terminal applications.
