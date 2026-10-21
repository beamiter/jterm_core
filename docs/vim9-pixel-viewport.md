# Semantic margins and a single pixel viewport transform

Render protocol 3 adds up to two real buffer rows above and below each supported
split. It retains source byte ranges, GUI syntax/search/property styling, signs,
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
launches actual Vim in a PTY with protocol 3. Legacy bridge tests retain protocol
1; delta tests retain protocol 2. Regression cases cover source identity, byte
mapping, split clipping, document edges, empty rows, property styles, hidden
unsupported text, source edits and fresh delta metadata.

## Deliberately not claimed

This is the tested producer/projection foundation, not complete pixel-direct
input in all four applications. Frontends still have their acknowledged-row
animation until their wheel admission, paint and mouse dispatch are migrated as
one transaction. Do not connect fractional input to painting alone: a click
must address the displayed source after asynchronous Vim acknowledgement, and
editing/selection, zoom, splits and fallback must cancel stale gestures safely.
No physical Wayland, hardware-GPU, Chinese-IME or VS Code latency result is
implied by these tests. Optional IDE chrome remains disabled by default.
