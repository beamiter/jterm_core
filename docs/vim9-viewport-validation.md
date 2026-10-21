# Semantic viewport validation — 2026-10-21

## Tested source and reproducibility

GitHub Actions run `39537350345`, job `119016954972`, validated the prepared
source against base `b8e340c3f47b37367a3c2fe56efcd6fbd641f701` on Ubuntu 24.04
with Rust stable and real Vim 9.1 in a PTY. The staging branch applies exact,
count-checked patches before formatting and testing. Its temporary workflow
and preparation scripts are deliberately not published to master.

The resulting source candidate is `f580b3feaf3bcee1778e4c0626fea9d862a398d0`.
The final publication preserves these tested source blobs:

| Path | Blob |
| --- | --- |
| `src/vim9.rs` | `f47c31931a2f773caa02e15d18bb9a80acefdd21` |
| `src/vim9_render.rs` | `aa918194d36a05922043dc264c6931b5295b9a27` |
| `src/vim9_viewport.rs` | `c7124ef0900123c606395230fadc8d6f6c9a1b5a` |
| `runtime/vim9/bridge.vim` | `64fa7b46ff644613b4452381010dd1c2bf7646d4` |
| `runtime/vim9/test_overscan.py` | `a8413c63914d4bdc961ed544cee80661773b6bed` |

Final publication additionally adds the overscan suite to regular CI, updates
the frontend pin-preparation workflow to check all targets without rewriting
source, and adds this validation record. Those workflow files are published
through the connected GitHub application, not the Actions token. No application
installation or restart on the user's machine is involved.

## Observed results

- `cargo test --locked --all-targets`: library 1,657 passed, 4 ignored;
  integration test binaries 4 passed and 1 passed, zero failures.
- The normally ignored real Vim round-trip test was explicitly run: 1 passed.
- `cargo clippy --locked --all-targets -- -D warnings`: passed.
- Warning-denied Rustdoc and formatted-source whitespace checks: passed.
- Real Vim Python suites: overscan 7, bridge 38, render delta 4,
  render windows 8, scroll 4, shell 3: all 64 passed.

The eight new Rust tests exercise fractional paint/hit/caret geometry, negative
margin rows, Unicode and expanded tabs, missing coverage, source-generation
invalidation, floating-point boundaries, explicit empty rows and invalid margin
metadata. The seven new PTY tests cover real semantic margins, hidden property
styles, unsupported hidden text, empty-row edits, independent splits, file edges
and protocol-3 delta metadata. Existing protocol-1/2 tests remain unchanged.

Earlier staging attempts caught and corrected an obsolete negotiation assertion,
a clone-on-Copy lint and a Vim E727 end-of-buffer fill-range error. Tests were
rerun after the fixes. No checks were disabled to obtain these results.

## Scope and remaining acceptance

This validates the producer and shared projection layer, not a complete
frontend pixel-direct scroll gesture. Four-frontend adoption must still couple
wheel admission, frame acknowledgement, pixel paint, source-byte mouse commands,
selection, caret and IME coordinates, with cancellation on edits and topology
changes. Updating a pinned core alone does not establish that integration.

The pin-preparation job checks every application target but does not execute
GUI tests. The repositories' broader CI, Nix packaging and existing frontend
lint failures are separate gates, not waived by a successful core upgrade.
Physical high-refresh input latency, Wayland/GPU presentation, Chinese IME and
a controlled VS Code comparison have not been measured in this validation.
