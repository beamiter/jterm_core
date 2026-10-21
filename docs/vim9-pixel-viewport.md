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

Only actual supplied source rows can be exposed. Resource budgets, file edges,
unsupported views or absent margins can limit the available interpolation.
No physical Wayland, hardware-GPU, Chinese-IME or VS Code latency result is
implied by these tests. Optional IDE chrome remains disabled by default.
