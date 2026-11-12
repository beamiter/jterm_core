# Independent input negotiation corpus

`crates/jterm_engine/tests/input_conformance.rs` sends literal bytes through the
bounded Ember parser and shared local dispatcher (plus an explicit fixture
adapter admitting only DECCKM enable/reset to `apply_ember_mode`), then passes the resulting
protocol state to the real input encoders. Expected replies and key bytes are
handwritten literals, not copied from a frozen encoder or computed by the
implementation's modifier formula.

Scope and references:

- Xterm control sequences: DECCKM / DECRQM, cursor-key modifiers,
  `modifyOtherKeys` and `formatOtherKeys`:
  <https://invisible-island.net/xterm/ctlseqs/ctlseqs.html>.
- Kitty progressive enhancement protocol: push/pop, set/OR/clear flags and
  query replies: <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>.
- Each lifecycle runs with chunk sizes 1 through 32, including byte-at-a-time
  input. Cancellation must leave negotiation inactive; queries must reflect
  state that actually controls encoding. Encoder output is checked after
  transitions, not only at the final reset.

The three tests cover DECCKM enable/reset with modified-key independence;
Kitty nested push/pop, flag operations, underflow and canceled push; and xterm
level/format transitions with host-supplied non-ASCII text. The checks use
Ember's existing printable-key repertoire. They do not claim full Kitty support
or policy compatibility for Frost, Forge or Anvil. No production behavior is
changed. Whole host CSI mode admission, shortcut routing, negotiation precedence, screen switching, IME
composition, OS input validation and PTY delivery are **not** exercised here.
These unit-level protocol tests are not native GUI or performance acceptance.

Run with:

```sh
cargo test --locked --manifest-path crates/jterm_engine/Cargo.toml --test input_conformance
```

Existing CI discovers this integration test through `--all-targets` on Rust
1.86, 1.89 and stable. A successful local run is not a substitute for exact-SHA
CI and consumer native checks.
