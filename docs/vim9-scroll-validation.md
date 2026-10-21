# Native Vim scrolling: bounded presentation and validation

This iteration changes the shared scroll helpers without changing their existing
frontend call signatures. Frontends must update their pinned core revision and
lockfile before these changes are part of their shipped binaries.

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

Tiny sub-line input still accumulates before Vim moves a row. True pixel-direct
manipulation needs overscanned semantic rows, a fractional presentation viewport,
and consistent hit-testing, caret and IME coordinates in all four renderers.
That work is not claimed by this change. Nor are wrapped lines, folds, platform
IME correctness or physical Wayland/high-refresh hardware established here.

A comparison against VS Code must use the same hardware, files, font, scale,
refresh rate and input device, with input-to-photon latency and p95/p99 frame
intervals, dropped frames, idle activity and memory recorded. This change does
not establish that comparative result or a universal SOTA claim.
