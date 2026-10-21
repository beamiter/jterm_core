# Source-bound direct pixel input

## Contract

`Vim9PixelScroll` integrates precision input once, in logical line units, and
presents a fractional offset immediately using sampled semantic margins. It
never adds a second synthetic momentum curve. Each split has at most one
`ScrollView` request in flight. The request carries window, buffer, changedtick,
topline and horizontal view identity. Vim publishes `scroll_ack` in the semantic
frame before replying; unrelated cursor frames do not count as acknowledgements.
The host rebases its remaining fraction by the actual acknowledged movement.
File-edge clamping clears accumulated debt. Missing margin coverage limits the
presented offset instead of inventing text. Geometry/source changes invalidate
old gestures. The host must cancel gestures before keyboard input, preedit,
zoom, topology changes and loss of native rendering ownership.

`Pointer` carries source UTF-8 bytes, not stale terminal cells. A primary press
and characterwise drag select in the real Vim buffer: yank, registers and undo
remain Vim-owned. Source generation, window focus, cursor and mode are checked
before continuing a gesture. Invalid UTF-8 boundaries, changed buffers, late
drags after Escape and cross-window drags are rejected, not replayed.

Optional protocol-3 window metadata advertises `native_pointer`, `scroll_ack`
and bounded `line_lengths`. Old producers default to no native-pointer support.
Custom mouse mappings, Select-mode, virtual columns, modifiers and multi-click
semantics are not reimplemented. Frontends must preserve compatibility input
for those cases. Visible empty lines and EOL whitespace now have explicit source
anchors; horizontally clipped source ends are not guessed.

The projection, mouse hit, caret and IME candidate rectangle must all use the
same offset. A core dependency update alone does not integrate a frontend.
The earlier viewport validation document describes the prior foundation stage;
this document describes the new shared input transaction layer.

## Atomic observation of paint and completion

Native input consumers use `Vim9Session::input_snapshot()` to obtain the render
frame and pending request IDs under the same worker lock. Keep that
`Vim9InputSnapshot` while reconciling scroll and pointer state; its
`command_pending(id)` refers to the instant captured with `frame`.

Calling `render_snapshot()` and then the live `command_pending()` can observe
an old frame followed by a completed response if the worker publishes an ack
between those reads. That falsely looks like a failed scroll and discards its
remaining fraction. The atomic snapshot preserves the order without waiting
on the worker or copying buffer lists, diagnostics or build output. It retains
only an Arc to the frame and the already bounded set of pending IDs.

Completion still does not imply success. Consumers must verify `scroll_ack`
and source identity as before. A retained snapshot remains immutable after a
later ack, response or disconnect. Frontends also retain their last actually
drawn frame for hit testing; a freshly received snapshot is not proof that the
user has seen it yet.

The atomic-observation follow-up was validated locally with 70 Vim9 Rust
tests, including retained pre-ack/ack/completed snapshots, preserved fractional
scroll debt and disconnect invalidation. Clippy on all targets and Rustdoc
passed with warnings denied; formatting and whitespace checks passed. The
existing 10-test real Vim pixel-input suite also passed against this bridge.

## Observed validation — 2026-10-21

Actions run `39540494697`, job `119026863605`, tested clean source candidate
`f80c65c4b43bc6c0a4484c3372236cd7000bba1e` on Ubuntu 24.04 with actual Vim 9.1.
Published Rust/Vim source blobs are identical to that tested candidate.

- Rust library: 1,667 passed, 4 ignored; integration binaries: 4 + 1 passed.
- Normally ignored real Vim round trip explicitly enabled: 1 passed.
- Clippy all targets with warnings denied, warning-denied Rustdoc, rustfmt
  and whitespace checks passed.
- Real Vim Python suites: new pixel input 10, overscan 7, bridge 38,
  render delta 4, render windows 8, scroll 4, shell 3: all 74 passed.
- Ten new Rust tests cover direct fractions, exact acknowledgement rebasing,
  failure/cancellation, bounded coverage, reversal, decimal boundaries,
  clamped views, and visible empty/EOL targets.

Frontend event routing and native GPU/GTK rendering require their own tests.
No hardware touchpad latency, high-refresh Wayland, Chinese IME or controlled
VS Code comparison is established by this core validation. No user application
installation or restart is performed by this change.
