# Native closed folds (render protocol 8)

Closed folds used to switch the whole editor to the compatibility view, so
the shared vimrc turned folding off in native editors. Protocol 8 paints a
closed fold as one native row in all four frontends, keeps every row below it
at Vim's own screen position, and scrolls across it with the same direct
pixel controller as ordinary text. Vim remains the editing authority: it
decides which folds exist, which are closed, and what the fold text is.

## What is painted

Each closed fold row is composed exactly as Vim's `fold_line()` draws it
(checked against Vim 9.1.0016 and current Vim sources):

- `Folded` covers the whole window width, including the gutter.
- An enabled sign column shows two blank `Folded` cells. Signs and sign line
  highlights are not drawn on a fold row.
- The number column shows the fold's first line, or its relative distance.
  `number` + `relativenumber` left-aligns the absolute number of the row that
  holds the cursor, exactly like ordinary rows (this alignment was wrong for
  ordinary rows before this change and is now fixed everywhere).
- The text is `foldtextresult()` for the fold, starting at the text column
  regardless of `leftcol`. A glyph that would not fit is dropped and the
  `fillchars` `fold:` glyph fills the rest of the row.
- `CursorLine` is not drawn on a fold row. `ColorColumn` and `CursorColumn`
  combine with it at their `leftcol`-shifted columns; `CursorColumn` appears
  on every fold row, including the one that holds the cursor.
- A linewise selection, or a characterwise one that covers the fold from
  column one through the end of its last line, replaces the text style with
  `Visual` from the text column on. Blockwise selections over folds keep the
  compatibility view.

Fold runs have `line: 0` and no byte range. They are display-only paint: no
hover, card or pointer target can resolve to a line hidden inside a fold.
Hidden lines are never sampled, styled or published; text properties are
queried only for the visible source segments, so diagnostics inside a large
closed fold cost nothing and do not consume the property paint budget.

Relative numbers count display rows, as Vim's `get_cursor_rel_lnum()` does:
a closed fold between the cursor and a row counts once.

## Wire format

Window metadata gains three optional fields, sent only to protocol 8 hosts:

| Field | Meaning |
| --- | --- |
| `display_lines` | First source line of every visible display row. Present only while a closed fold is in the visible or margin rows; empty means the legacy `topline + row` mapping. |
| `fold_ends` | Parallel to `display_lines`: the fold's last line, or zero for an ordinary row. |
| `scroll_rows` | Display rows actually moved by the request named in `scroll_ack`. |

Margin rows (`overscan`) are display rows too and carry `fold_end` when they
show a closed fold. `line_lengths` has one entry per visible display row and
`botline` is the last line covered by the last visible row.

`Vim9RenderFrame::valid()` accepts a row map only when it is a contiguous
chain: the first row is `topline`, each row starts right after the previous
row's last line, the visible chain ends at `botline`, and both margins
continue the chain outward. A source-addressed run on a fold row, a fold
metadata field from a pre-8 producer, or `native_pointer` on a folded split
is rejected.

`Vim9RenderWindow` exposes the map without frontend arithmetic:
`visible_rows()`, `display_rows()` (margins included), `display_row_of(line)`
(a hidden line resolves to its fold row) and `topline_shift(&next)` (the
display-row distance between two frames, or `None` when neither sample covers
the other's topline). `Vim9RenderRun::repeated_glyph()` identifies a filler
run that GPU painters place per cell.

## Input

Source-bound scrolling stays enabled. `<C-E>`/`<C-Y>` already treat a closed
fold as one row, so a request for N rows moves Vim's topline past an entire
fold. The bridge counts the rows it really moved in the split's own fold
state and reports them as `scroll_rows`; `Vim9PixelScroll` uses that count
instead of the topline delta, so one wheel notch or touchpad stroke across a
2,000-line fold continues smoothly instead of snapping. Margins supply the
fold row itself, so the transition shows real paint.

Pointer clicks and drags in a split that shows a closed fold go to Vim's own
mouse handling (`native_pointer` is false), so opening a fold with a click,
dragging across a fold and every fold-related mapping behave exactly as in
Vim. Frontend wheel transitions on the compatibility route (for example in
Visual mode) measure distance with `topline_shift()`.

## Compatibility

Hosts negotiating protocol 7 or older keep the previous behavior: a closed
fold in the visible area uses the compatibility view, and margin folds only
drop the margins. `foldcolumn`, blockwise selections over folds and fold text
containing control or bidirectional characters still use the compatibility
view.

## Validation

- `python3 -m unittest -v test_render_folds` (from `runtime/vim9`) compares
  every painted cell of fold rows with Vim's own `screenstring()` for numbers,
  relative/hybrid numbers, sign columns, custom `foldtext` with wide glyphs,
  `fillchars`, horizontal scrolling with `colorcolumn`/`cursorcolumn`, inactive
  splits and fold toggles without buffer changes; checks Visual/CursorLine
  attributes against `screenattr()`; scrolls across a 279-line fold with
  `scroll_view`; and runs the shipped vimrc on protocols 7 and 8.
- Rust unit tests cover the row map, every validation rejection, fold-aware
  viewport targets and the `scroll_rows` acknowledgement.

These tests exercise real Vim and the shared core. They do not measure GPU
frame time or input latency on a physical display.

## Cost

Building the row map adds a handful of `foldclosedend()` calls per visible
row. The same change removed a larger cost: scrolling used to deep-copy every
surviving cached glyph just to change its row, and now retargets the row in
place. `benchmark_render.py --lines 50000 --samples 200` (three interleaved
rounds, previous bridge vs this one, no folds) measured Vim CPU per Ctrl-E
scroll falling from 7.7–7.95 ms to 6.25–6.7 ms with one window and from
9.45–9.9 ms to 8.95–9.45 ms with two splits.
