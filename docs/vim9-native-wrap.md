# Native soft wrap (render protocol 9)

A line wider than its window used to switch the whole editor to the
compatibility view, so the shared vimrc forced `nowrap` in native editors.
Protocol 9 paints every wrapped continuation row natively in all four
frontends, keeps each row at Vim's own screen position, and scrolls through
wrapped lines, including `smoothscroll` partial lines, with the same direct
pixel controller as ordinary text. Vim stays the layout authority: the bridge
reproduces Vim's line breaking and checks it against Vim before it paints.

## What is painted

Continuation rows follow Vim's `win_line()` (checked against Vim 9.1.0016
sources and `screenstring()`/`screenattr()` probes):

- The sign column of a continuation row is blank `SignColumn`. The number
  column is blank, in the highlight Vim gives that line's continuation rows
  (`LineNr`, `LineNrAbove`/`LineNrBelow`, or `CursorLineNr` as
  `cursorline` and `cursorlineopt` decide). The `cpoptions` `n` flag, which
  moves wrapped text into the number column, keeps the compatibility view.
- `showbreak` is drawn before or after the `breakindent` indent, as
  `breakindentopt` `sbr` selects. With `linebreak`, `breakindent` or
  `showbreak` the bridge takes every glyph's row and column from Vim's own
  `virtcol()`, so `breakat` and all `breakindentopt` values match Vim.
- A double-width glyph that does not fit at the end of a row moves to the
  next row and leaves Vim's `>` filler in the last cell. A tab that crosses a
  row boundary is split across both rows, as Vim draws it.
- With `smoothscroll`, the first row of a partly scrolled topline starts with
  `<<<` in `NonText`, and the hidden rows become the top margin. With
  `number` + `relativenumber` the skipped row shows Vim's dashes.
- `display=lastline` and `display=truncate` draw `@@@` exactly where Vim does;
  with an empty `display` an unfinished last line becomes `@` rows. The
  `fillchars` `eob:` and `lastline:` glyphs are respected.
- Visual selections, search and match highlights, `CursorLine`, syntax and
  text properties continue across continuation rows. The Visual EOL cell
  appears after the last part only.
- `colorcolumn` and `cursorcolumn` address virtual columns, and every
  wrapped row spans exactly one text width of them (`showbreak`/indent cells
  and `linebreak` padding included). A guide therefore shows on the row
  that holds its column: inside text, on `showbreak` cells, in `linebreak`
  padding and after the end of the line on its last row, but not on the `>`
  filler, which Vim writes with plain `NonText`. `cursorcolumn` follows the
  cursor's own row and cell (`screenpos()`). As in Vim, lines in a Visual
  selection and cells under a search match get no guide.
- Rows past the end of the buffer are filled across the whole width in
  `EndOfBuffer` after the `eob:` glyph, as Vim's `win_draw_end()` does.

Relative numbers count lines, not rows, as Vim does; closed folds between
wrapped lines still count once (protocol 8).

## Wire format

Window metadata gains three optional fields, sent only to protocol 9 hosts:

| Field | Meaning |
| --- | --- |
| `wrap_parts` | Parallel to `display_lines`: which continuation row of its line each visible row shows, from zero. |
| `wrap_rows` | Parallel to `display_lines`: how many screen rows that line takes. |
| `skipcol` | Vim's `smoothscroll` offset of the topline (`winsaveview().skipcol`). |

Margin rows (`overscan`) carry `part` and `parts` too, so a touchpad stroke
can reveal half of a wrapped line from real paint. `display_lines` and
`fold_ends` are sent whenever a wrapped line or a closed fold is in the
visible or margin rows. `line_lengths` keeps one entry per visible row.

`Vim9RenderFrame::valid()` accepts the map only as a contiguous chain: each
row is the next part of the same line, or part zero of the line after the
previous row's last line; the first row is `topline`, at a non-zero part
exactly when `skipcol` is non-zero; the visible chain ends at `botline`, and
both margins continue it outward. Fold rows always have one part. Wrap
metadata from a fold-only or legacy producer is rejected.

`Vim9DisplayRow` gains `part`, `parts` and `ends_line()`.
`display_row_of_part(line, part)` finds one continuation row,
`same_first_row(&other)` compares topline and `skipcol`, and
`topline_shift(&next)` counts wrapped rows as well as folds, so frontends
animate a one-row `smoothscroll` move that leaves `topline` unchanged.

## Input

`<C-E>`/`<C-Y>` with `smoothscroll` move one screen row inside a wrapped
line. Each `scroll_view` request now names the `skipcol` the host saw; the
bridge rejects a request whose topline or `skipcol` changed, counts the rows
it really moved (folds once, wrapped parts each) and reports them in
`scroll_rows`. `Vim9PixelScroll` treats a `skipcol` change like a topline
change, so an unrequested move retires the gesture instead of jumping.

Native scrolling needs `smoothscroll` in a split that shows wrapped rows:
without it `<C-E>` moves whole lines, so such a split scrolls through Vim's
compatibility input (the shared vimrc sets `smoothscroll`).

Continuation rows carry exact source bytes, so the native pointer stays on
in a wrapped split: a click or drag on any continuation row, including one
while a touchpad scroll is between rows, lands on that byte and drags make
Vim's own Visual selection. The empty area after a line's text is its
end-of-line target on the last part only; the `showbreak`/indent area and
`>` fillers are not targets. A split that shows a closed fold, visible or in
a margin, still sends clicks to Vim's own mouse handling. Hover and card
targets use the same byte map.

## Compatibility

Hosts negotiating protocol 8 or older keep the previous behavior: a line
that wraps in the visible area uses the compatibility view. The shared vimrc
keeps `wrap` until the bridge reports the host's protocol and turns it off
only for older hosts.

These cases still use the compatibility view: `list` on wrapped lines,
virtual text on a line that wraps, blockwise Visual with wrapped lines,
a tab that crosses a row with `showbreak` or `breakindent`, `cursorlineopt=screenline` on a wrapped cursor
line, a wrapped line taller than its window, a `skipcol` that is not a whole
row, control or bidirectional text, zero-width or complex glyphs at a row
edge, and lines over 16 KiB or 4,096 characters.

## Validation

- `python3 -m unittest -v test_render_wrap` (from `runtime/vim9`) compares
  every painted cell with Vim's own `screenstring()` for plain wrapping with
  numbers and signs, inactive splits with closed folds between wrapped lines,
  wide glyphs, tabs and combining characters at row edges, `linebreak`,
  `breakindent` and `showbreak` combinations, `smoothscroll` first rows, every
  `display` mode, and `scroll_view` through wrapped rows; checks Visual,
  search, `eob` and `CursorLineNr` attributes against `screenattr()`; and
  checks that protocol 8 keeps the compatibility view.
- Rust unit tests cover the part-aware row map, every validation rejection,
  `topline_shift` across parts, the end-of-line target and the `skipcol`
  acknowledgement.
- The vimrc suite checks that native windows keep `wrap` on protocol 9 and
  older hosts get `nowrap`.

These tests exercise real Vim and the shared core. They do not measure GPU
frame time or input latency on a physical display.

## Cost

Line layout is computed once per line and cached with the line, like other
glyphs; scrolling reassigns rows without recomputing it. On a 20,000-line
Rust file whose lines take two rows in a 120-column PTY (three interleaved
rounds, 200 Ctrl-E each, protocol 9, `nosmoothscroll`), Vim CPU per scroll was
8.45–9.1 ms with `wrap` and 13.85–15.6 ms with `nowrap`; the median
key-to-frame time, including Vim's 16 ms coalescing timer, was 27–29 ms and
34–36 ms. A wrapped view shows fewer distinct lines, so it costs less per
scroll than the clipped `nowrap` view of the same file.

Follow-up (same day): each screen row's runs are now JSON-encoded once and
reused for the keyframe, the change test and the row delta (previously the
runs were encoded up to three times per frame), and glyphs that touch no
guide column skip guide segmentation. Same file and method, two interleaved
rounds against the previous bridge: `nowrap` 13.4–14.55 → 12.8–13.7 ms,
`wrap` 9.1–9.45 → 8.15–8.35 ms, and `nowrap` with `colorcolumn=80,120` plus
`cursorcolumn` 29.65–32.1 → 25.4–27.9 ms. With guides, `wrap` costs
16–17 ms per scroll.
