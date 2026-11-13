# Independent SGR state and status-query corpus

`crates/jterm_engine/tests/sgr_conformance.rs` sends hand-authored control bytes
through the bounded EmberV1 parser and shared semantic dispatcher. It checks both
the actual rendition at each DECRQSS reply and independently written reply bytes.
No legacy execution trace or unchunked run supplies expected values.

The scenarios cover:

- SGR bold/dim/italic/underline/blink/inverse/strike and selective reset (22 clears
  both bold and dim, while the other reset parameters clear their own attributes).
- Bright named colors, indexed colors and RGB, including colon syntax with omitted
  color-space ID; 39 and 49 independently restore default foreground/background.
- `4;3` means single underline plus italic, whereas `4:3` selects curly underline;
  double underline is reported as 21 under the existing Ember reply policy.
- Explicit SGR 0 and omitted SGR parameters reset visual attributes and colors,
  without clearing the separate DECSCA protection state.
- CAN/SUB cancel incomplete SGR without applying its reset or color parameters.

Each test uses chunk sizes 1 through 32 and repeated empty feeds. Every stage
requires exactly one reply and checks its intermediate state, parser quiescence,
cursor position and unchanged seeded cell grid. Global-background notifications
are checked against the current background; host delivery is not modeled.

References: ECMA-48 fifth edition (SGR), XTerm Control Sequences (SGR, DECRQSS),
and DEC VT510 Reference Manual (DECSCA and DECRQSS):

- https://ecma-international.org/publications-and-standards/standards/ecma-48/
- https://invisible-island.net/xterm/ctlseqs/ctlseqs.html
- https://vt100.net/mirror/mds-199909/cd3/term/vt510rmb.pdf

The state assertions are semantic expectations; the exact reply ordering and
omission of default color fields are **Ember compatibility policy**, not a claim
that the standards require identical serialization. The returned SGR string is
not tested as a state-restoration program on an arbitrary non-default terminal.

This adds no production behavior. It does not validate selective erasure of
protected cells, underline color support, all malformed color parameters,
Frost/Forge/Anvil routing, PTY replies, native pixels or full host acceptance.

Run:

```sh
cargo test --manifest-path crates/jterm_engine/Cargo.toml --locked --test sgr_conformance
```
