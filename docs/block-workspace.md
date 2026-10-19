# Block workspace responsiveness

The four terminal frontends share pane geometry and search-budget semantics.
Their native widgets, block identities, selection, and output projections stay
in the owning frontend.

## Move to the pane beside the active pane

`pane_layout::directional_focus_target` consumes visible pane rectangles in one
coordinate space. A candidate must sit beyond the active pane's requested edge
and overlap it on the perpendicular axis. A tall pane alongside two shorter
panes therefore cannot intercept Up or Down just because its center is offset.

Candidates are ranked by edge distance, amount of perpendicular overlap,
center alignment, then stable session identity. Ties are independent of widget
enumeration order. The same rule handles negative origins, logical units,
physical pixels, and normalized layout geometry. The rounding tolerance is at
most one unit and at most one percent of the smaller pane's extent on the
navigation axis, so it cannot consume an entire small pane.

Empty, negative, non-finite, overflowed, and numerically collapsed rectangles
are ineligible. An invalid active allocation or a missing neighbor returns
`None`; callers retain their existing focus and zoom policy. Frontends must
exclude hidden widgets when assembling the rectangle list.

## Let long searches yield

The cross-block palette keeps an 8 MiB byte budget per cooperative slice, with
an 8 ms time target. Live Find keeps a 4 MiB byte budget and a 4 ms target. The
targets were previously 48 ms and 12 ms. Shorter slices return control to the
frontend event loop more frequently during a long history scan.

These are cooperative targets, not hard latency guarantees. A frontend checks
the budget between scan units; an individual regex call, text extraction, or
widget update can exceed the target. Matchers should be prepared once per
query rather than recompiled for every continuation.

The existing resume cursor preserves record, surface, line, and occurrence.
Only a continuation with the live search generation may apply its results.
Reaching a time or byte budget means incomplete search; reaching the result
limit means capped results. Neither condition means there were no matches in
the unexamined part of the history.

## Verification

The geometry tests exercise uneven nested panes, all four outer edges, negative
origins, logical-to-physical scaling, stable ties, and invalid allocations. The
search tests cover UTF-8 byte boundaries, generation cancellation, constructors,
and continuation metadata. Run them with:

```sh
cargo test --locked --lib pane_layout
cargo test --locked --lib cross_block_search
```

Frontend integration still needs the native GTK, egui, or iced widget/event
path. Core tests alone do not establish GPU rendering performance or a desktop
session's observed frame latency.
