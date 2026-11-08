# Architecture checkpoint packaging

The architecture candidate is packaged as a coordinated checkpoint across the
existing repositories. Frontend manifests pin exact core commits, not branch names.
This is development work, not a release or a completed shared terminal runtime.
The earlier milestone reports retain their original local-only validation scope.

## Source layout and dependency identity

- `jterm_core` remains the root package, version 0.2.0 and Rust 1.86 minimum.
- `crates/jterm_engine` is a separately locked, independent Cargo workspace,
  version 0.1.0 and Rust 1.86 minimum. Its only external dependency is smallvec.
  It has no core, GUI, PTY, Agent, filesystem, network or executor dependency.
- Ember and Frost pin both package names to the same complete Git commit in the
  existing `beamiter/jterm_core` repository. Forge and Anvil pin only core.
  No new repository or crates.io publication is required.
- Development pins must be updated only after the core/engine commit exists.
  Publish that exact core commit before frontends that depend on it. Never
  publish manifests containing the provisional `../jterm_engine` dependency.
- `scripts/family-cargo.py` provides explicit local overrides and private
  manifests/locks. It prefers the nested engine, supports older sibling layouts,
  and runs native tests from the original checkout. Overrides follow the Cargo
  subcommand so Clippy receives the same candidate as Cargo check/test.

## Standalone checks

From this repository:

```sh
cargo test --locked --manifest-path crates/jterm_engine/Cargo.toml
cargo +1.86.0 test --locked --manifest-path crates/jterm_engine/Cargo.toml
cargo clippy --locked --manifest-path crates/jterm_engine/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path crates/jterm_engine/Cargo.toml -- --check
python3 crates/jterm_engine/scripts/check_boundary.py --engine-only
python3 scripts/test-family-engine.py
```

CI runs the engine separately on stable and Rust 1.86. The security audit covers
all three independent locks: root core, engine and the companion-host harness.
The full engine boundary checker also requires Ember and Frost checkouts beside
this repository; it verifies the actual frontend adapters and source identity.

## Source distribution

Cargo fetches both package names from the same immutable Git source. The engine
ships within that source tree, including its licenses, tests and separate lock.
Frontend materialization helpers therefore need no independent engine checkout.
Snapshot tooling includes `crates/` when present and excludes build products.
Existing archived snapshots remain historical evidence, not current build inputs.

When repinning Forge or Anvil, regenerate Flatpak Cargo sources using the pinned
generator and update the Nix full source-tree NAR hash. A gzip/tar checksum is not
a NAR hash. Manifest, dev-dependency, lock, policy and packaging identities must
agree; a local path override is not exact-publication validation.

## Compatibility and acceptance limits

The extraction preserves separate Ember and Frost legacy parsing profiles and
existing host policy. It is not a promise of identical frontend semantics.
Native GUI/IME acceptance, a complete SessionRuntime, persistence and Tauri
rendering remain outside this candidate. Runtime experiments are not part of
this source tree. Earlier reproduced environment failures remain disclosed in
`validation-results.md`; packaging checks do not convert those suites to green.
