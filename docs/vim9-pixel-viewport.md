# Semantic margins and a single pixel viewport transform

Render protocol 4 adds up to eight real buffer rows above and below each supported
split; protocol 3 hosts retain their original two-row margins. It retains source byte ranges, GUI syntax/search/property styling, signs,
line numbers and explicit empty rows. The producer does not move Vim's window,
cursor, focus, mode, jumplist or buffer to sample these rows. Old protocols 1/2
remain accepted and do not produce margin rows.

Margins are bounded separately and are not added to the terminal-owned screen
row coordinates. Each edge row has a signed viewport-relative row index. Its
nested runs use row zero. The validator rejects out-of-range, duplicated,
unsorted, cross-window or inconsistent source rows. Margins are replaced with
window metadata in both keyframes and deltas, never silently inherited after a
new source generation. Unsupported hidden text disables margins only; an actual
unsupported visible state still uses the compatibility renderer. A size budget
first drops margins before giving up the visible native frame.

`Vim9Viewport` projects visible and margin runs into one clipped logical-pixel
space. Paint geometry, source-byte hit testing, anchor re-resolution and caret /
IME rectangles share this transform. Raw and clipped rectangles remain separate:
a half-visible glyph must be clipped, not stretched. Negative top rows work even
when the split begins at screen row zero. A transform with insufficient sampled
coverage fails closed rather than extrapolating document contents.

## Validation

Run the regular Rust suite and `python3 runtime/vim9/test_overscan.py`. The latter
launches actual Vim in a PTY with protocols 3 and 4. Legacy bridge tests retain protocol
1; delta tests retain protocol 2. Regression cases cover source identity, byte
mapping, split clipping, document edges, empty rows, property styles, hidden
unsupported text, source edits and fresh delta metadata.

## Wheel presentation

All four frontends use the source-bound pixel controller for supported views.
Trackpad pixels remain direct. Discrete notches advance continuously over
100 ms using the GUI frame clock, independent of metadata polling or arrival
of a new Vim frame. Repeated notches keep finite, independent completion
deadlines; reversal cancels the unplayed old direction. Eight-row margins
provide real text for ordinary wheel bursts while a source-checked request is
in flight. Exact acknowledgements rebase the same transform without replaying
motion. A press freezes future animation and uses the displayed source bytes.
Editing, preedit, zoom, splits and fallback cancel obsolete gestures.

The controller tracks requested motion separately from the motion actually
displayed. A fast touchpad event can exceed the eight supplied margin rows;
only the displayed whole-row portion is committed to Vim. An exact reply
subtracts that same portion from both coordinates, retaining the identical
source-pixel position and fractional offset. It must not expose the remaining
backlog immediately or snap text back to a row boundary.

When new source rows arrive, the blocked remainder advances on the GUI frame
clock with a bounded catch-up curve (40 ms time constant, at most 120 rows/s).
The capped exponential is integrated in elapsed time, so sampling at
60/120/144/240 Hz does not change its trajectory. This curve applies only to
motion previously blocked by unavailable rows. Ordinary touchpad deltas,
including 1 px and 0.25 px, still move text, caret and IME anchors directly;
the OS's momentum is not eased a second time. Large bursts may take longer to
catch up rather than jumping across unseen text.

Reversal retires only invisible old-direction backlog and immediately moves
the displayed image within its source coverage. While a request is in flight,
the transform stays inside both the old and replacement viewports' margin
budget. A pointer press freezes both wheel animation and blocked catch-up.
At file boundaries, a rejected forward movement cannot consume a newly queued
reverse gesture. Without supported margin rows, the compatibility whole-line
route remains available; the GUI never invents text outside its sampled data.

Regression checks cover quarter-pixel events across whole-row acknowledgements,
exact origin preservation after margin saturation, forward/reverse catch-up,
frame-rate independence and idle settling. Frontend tests additionally check
the projected/painted text and caret/IME coordinates; the GTK fixtures exercise
real Vim in a native window.

Only actual supplied source rows can be exposed. Resource budgets, file edges,
unsupported views or absent margins can limit the available interpolation.
No physical Wayland, hardware-GPU, Chinese-IME or VS Code latency result is
implied by these tests. Optional IDE chrome remains disabled by default.
