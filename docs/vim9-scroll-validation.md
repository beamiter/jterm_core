# Native Vim scrolling: bounded presentation and validation

The shared scroll helpers and Vim runtime preserve their existing frontend call
signatures and wire protocols. Frontends must update their pinned core revision
and lockfile before these changes are part of their shipped binaries. Local
coordinated builds use `scripts/family-cargo.py` without modifying release pins.

## Runtime pipeline follow-up — 2026-10-22

Fast browsing could exhaust the eight real margin rows while waiting for a
source-bound scroll acknowledgement. The frontend then correctly stopped at
its sampled boundary, producing a visible pause until the next frame arrived.
Profiling distinguished that wait from Vim's cursor relocation: in a workload
that moved the actual cursor on every scroll, `ScrollWindow()` itself took
about 0.04 ms per call, while repeated semantic composition dominated CPU time.

Four changes reduce that delay in the producer shared by all four frontends:

- A typed local paint list feeds `AddRenderRun()`. Passing `frame.runs` through
  `dict<any>` made Vim infer/check the entire growing list at every append,
  creating quadratic work. Changing the parameter to `list<any>` alone did not
  avoid the runtime inference; keeping the argument statically typed does.
- Source-line caches also retain the resolved default/syntax/property/match
  style. CursorLine, sign line highlights and Visual selection still compose
  their current overlays in the original precedence order. Theme cache keys use
  a generation counter instead of repeatedly embedding the complete highlight
  and color-name tables. Theme and source invalidation rules remain active.
- An immediate paint consumes its queued 16 ms render timer. Source scrolling
  no longer composes a duplicate viewport just after publishing its ack.
- A validated `scroll_view` writes its semantic ack and response together.
  Separate small channel writes exhibited a roughly 40 ms delayed-ACK/Nagle
  tail. Message order and exact request ids are unchanged. Batching starts
  after input validation and is restricted to this scroll transaction;
  authentication, deferred saves and nested input callbacks keep their existing
  delivery semantics. Errors flush the batch before propagating.

Anvil and Forge additionally trim cold Pango layouts when the shape cache is
full, retaining visible text, all split margins and completion shapes. They no
longer clear the entire cache and reshape a warm screen periodically. Font,
scale and ligature changes still invalidate layouts normally.

### Measurement

`runtime/vim9/benchmark_scroll.py` uses a real Vim 9.2 PTY and protocol 5. The
30×120 viewport shows Rust syntax, Chinese, combining text and emoji, with
relative line numbers, CursorLine and `scrolloff=5`. Each of 160 requests moves
the active cursor; the two-window case retains a stationary sibling. Baseline
runtime: core `58d1c2c`.

| Request to ordered semantic ack and response | Baseline median / P95 | Optimized median / P95 |
| --- | --- | --- |
| One window, 10,000 lines | 167.4 / 243.1 ms | 16.3 / 20.0 ms |
| Two windows, 10,000 lines | 265.6 / 393.0 ms | 26.8 / 33.5 ms |

Vim CPU per request fell from approximately 173 to 15 ms in one window and
286 to 25 ms in two windows. The optimized 100,000-line replay also completed
160 ordered acks per case, with median 17.9 / 31.2 ms. These measurements are
producer/transport latency on this development host, including JSON decoding,
not displayed frame intervals or input-to-photon latency. The GUI frame clock
continues moving through real margin rows independently of acknowledgement
cadence. No physical VS Code or high-refresh display comparison is claimed.

```sh
cd runtime/vim9
VIM9_TEST_VIM=/absolute/path/to/vim python3 benchmark_scroll.py --lines 10000
# Use an exported baseline runtime for a controlled comparison:
VIM9_TEST_VIM=/absolute/path/to/vim python3 benchmark_scroll.py --lines 10000 --runtime /tmp/baseline.vim
```

The real-Vim suite ran 114 tests: 113 passed and one completion-metadata
capability check was skipped.
It includes a sustained forward/reverse stream checking every semantic ack
precedes its response, stable source generations, rejection of stale requests
and recovery after errors. Existing tests cover syntax/theme invalidation,
properties, signs, Visual precedence, Unicode editing/saving, completion,
independent splits and source-pointer input. The shared Rust Vim suite passed
103 tests with its separate opt-in real-PTY test ignored.

## Behavior

The old animation restarted a 100 ms cubic tail whenever a new acknowledged
viewport arrived. The new animation keeps each acknowledged movement's own
completion time. Sustained input no longer renews all previous motion on every
frame. A zero-delta frame cannot keep an old animation alive.

There are at most 64 impulses per split. At extreme input rates, the oldest two
are compacted at the current sampled position, with their original latest
completion deadline, rather than granting another full tail. The output remains
bounded to 8192 logical pixels. Invalid input and backwards clock transitions
cannot leave old motion active. The frame clock stays alive across a transient
zero crossing of opposing impulses, so a reversal cannot freeze halfway through.
This is presentation of acknowledged rows, not additional touchpad momentum.

Fractional wheel input keeps a bounded least-recently-used split cache. Opening
another split at capacity evicts only an idle entry instead of clearing all
ongoing gestures. Floating-point noise at an integer boundary is snapped within
a few machine epsilons, so ten 0.1-line events produce one line. Meaningful
fractional movement is not rounded away. `discard(window)` allows a host to
forget a closed or repurposed split independently.

## Reproducible checks

The module contains ten unit tests and can be tested without GUI dependencies:

```sh
rustc --edition=2021 --test src/vim9_scroll.rs -o /tmp/jterm-vim9-scroll-tests
/tmp/jterm-vim9-scroll-tests
```

The regular CI also runs the tests through Cargo on the minimum Rust version
and stable, plus the real Vim bridge, shell-entry and split-scrolling Python
suites. Formatting, Clippy and warning-denied Rustdoc remain enforced.

The constant-velocity unit test replays 600 logical pixels per second at 60,
120, 144 and 240 acknowledgement events per second. It compares the same scalar
viewport trajectory against the previous restarted-tail formula and requires
at least a 10% reduction in steady presentation offset at all four rates.
An independent numerical replay gives approximately 14%, 19%, 20% and 22%
reductions respectively. These are model results, not input-to-photon latency,
GPU frame times or physical monitor measurements.

Further cases cover fractional accuracy, split eviction, continuity, original
deadlines, zero-crossing liveness, sampling independence, 5,000 high-rate events,
invalid values, clock rollback and settling after input stops.

## Remaining acceptance gates

The original helper-only iteration did not include direct pixel input. The
subsequent source-bound viewport and frontend integration are documented in
[the pixel viewport contract](vim9-pixel-viewport.md). Platform IME
correctness and physical Wayland/high-refresh hardware still have their
documented compatibility and acceptance boundaries. Closed folds are native
display rows since render protocol 8 ([native closed folds](vim9-native-folds.md)),
and wrapped lines, including `smoothscroll` partial lines, since protocol 9
([native soft wrap](vim9-native-wrap.md)).

A comparison against VS Code must use the same hardware, files, font, scale,
refresh rate and input device, with input-to-photon latency and p95/p99 frame
intervals, dropped frames, idle activity and memory recorded. This change does
not establish that comparative result or a universal SOTA claim.
