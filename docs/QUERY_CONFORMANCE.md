# Independent mode-query wire corpus

`crates/jterm_engine/tests/query_conformance.rs` drives the bounded production
parser into the production `dispatch_local` function using literal query bytes.
Expected reply byte strings are handwritten, not obtained from a previous
implementation, whole-feed candidate run or stored golden. Existing frozen Ember
DECRQM differential tests remain separate compatibility evidence.

## Standards and supported slice

The DEC VT510 programmer manual defines ANSI `CSI Pa $ p` and DEC `CSI ? Pd $ p`
queries, and their corresponding `CSI Pa ; Ps $ y` / `CSI ? Pd ; Ps $ y` reports.
Status 1 means set, 2 reset, 0 not recognized and 4 permanently reset. Sources:

- https://vt100.net/docs/vt510-rm/DECRQM.html
- https://vt100.net/docs/vt510-rm/DECRPM.html
- PDF mirror, DECRPM section, printed page 5-95:
  https://vt100.net/mirror/mds-199909/cd3/term/vt510rmb.pdf

This suite exercises only Ember-compatible semantics, **not** automatic Frost,
Forge or Anvil semantic routing or a universal capability declaration. It checks:

- Actual insert mode (ANSI 4) and origin mode (DEC 6) return status 1/2 and the
  exact appropriate wire prefix. Deliberately contradictory protocol bits expose
  a query consulting the wrong state owner.
- Supported cursor visibility (DEC 25) reports its actual protocol bit as 1/2.
  This is not a claim that any renderer displayed a cursor.
- Ember's existing local capability policy reports ANSI 20 permanently reset,
  private 1016 unrecognized, and unrecognized 65000 in both namespaces as 0,
  even when storage contains such bits. The standard defines report meanings;
  **these particular supported/unsupported choices are local policy**, not DEC
  mandates. No additional mode is advertised or implemented by this change.
- Lookalike prefix/final/intermediate combinations are not handled as DECRQM.

Every query case runs at every single feed split and bytewise with repeated empty
feeds, including splits through `$p`. Reply order and the absence of extra effects
are asserted. Queries leave sentinel snapshots of actual owners, current drawing
state, cursor, both grids and protocol unchanged. The snapshot is not exhaustive
host-state equivalence; frozen host oracles cover different integration concerns.

## Explicit exclusions and next risks

The suite deliberately does not promote legacy first-parameter-only behavior or
colon/overflow normalization to a standards requirement. DEC describes one queried
mode at a time; malformed or multi-mode query compatibility requires separate
policy work. There is no VT52 implementation claim, no status-3 fixture (no known
permanently-set mode in this slice), no claim to support private 1016, and no new
mouse/keyboard/clipboard functionality. Grid mutation, actual reply transmission
through PTY and native TUI behavior remain separate acceptance tests.

The full legacy supported-mode list still needs capability-by-capability review
against each host, not only the existence of stored protocol flags. In particular,
passing this suite does not certify every advertised mouse mode or all four hosts.

```sh
cargo test --manifest-path crates/jterm_engine/Cargo.toml --locked --test query_conformance
```
