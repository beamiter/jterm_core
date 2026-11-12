# Independent TUI state and status-query scenarios

`crates/jterm_engine/tests/tui_state_conformance.rs` replays hand-authored control
streams through the real bounded parser and Ember-compatible semantic dispatcher.
Unlike parser-token checks, it observes real cursor coordinates, scroll margins,
character-protection state and emitted reply bytes after preceding state changes.
Neither legacy code nor whole-feed output supplies expected values.

## Normative mapping

- CUP/relative CUU/CUF followed by DSR/CPR: a 6x12 screen moves to row 3/column 4,
  up one row, right two columns, then reports row 2/column 6. Internal coordinates
  are zero-based; wire coordinates are one-based. DSR 5 reports `CSI 0 n` and CUP
  with omitted parameters returns to 1/1. No pending-wrap or origin-mode behavior
  is inferred from this scenario.
- DECSTBM changes margins to 2..5; DECRQSS `r` returns those current margins.
  Restoring default margins reports 1..6. DECSCA 1 selects protected characters;
  DECSCA 0 selects unprotected, and DECRQSS `"q` reports each state at that point.
- CAN/SUB cancel an incomplete status-string query; only a subsequent complete
  query replies. A manually counted single reply detects accidental completion.
- An unsupported DECRQSS selector uses XTerm's `DCS 0 $ r ST` response, without
  reflecting the arbitrary selector text, and preserves cursor state.

References: ECMA-48 fifth edition for CUP/CUU/CUF/DSR; XTerm Control Sequences
patch 411 for the implemented DECRQSS success/failure convention and selectors:
https://invisible-island.net/xterm/ctlseqs/ctlseqs.html
https://invisible-island.net/xterm/ctlseqs/ctlseqs.pdf
DEC VT510 manual, DECRQSS/DECRPSS/DECSTBM/DECSCA:
https://vt100.net/mirror/mds-199909/cd3/term/vt510rmb.pdf

This suite explicitly follows the XTerm success/failure convention rather than
claiming every terminal profile emits identical replies.

## Coverage and limits

Every single input split and bytewise feeds with repeated empty feeds are tested.
State snapshots are taken at every reply, so a correct final state cannot hide a
wrong intermediate transition. The complete primary cell grid, seeded with a
Unicode sentinel, must remain unchanged by these control-only streams. Unexpected
row, bell or background effects fail the test; cursor-observer effects are allowed
but their host delivery is not modeled here.

This suite executes real Rust engine state, **not a real GUI/TUI process**. It
covers only the Ember semantic policy, not Frost/Forge/Anvil routing or capabilities.
Host PTY transmission, origin mode, wrap behavior, alternate-screen restoration,
selective erasure of protected cells, SGR roundtrip completeness, native pixels,
font/grapheme display and actual Vim interaction need separate tests. It adds no
production support and does not replace native CI or old compatibility oracles.

```sh
cargo test --manifest-path crates/jterm_engine/Cargo.toml --locked --test tui_state_conformance
```
