# Shared terminal acceptance: engine slice

Use the pinned checkout, Python 3.11+, Git, Cargo, and the engine lock's dependencies:

    python3 scripts/terminal-family-evidence.py --offline --output /tmp/engine-acceptance

Omit `--offline` to use Cargo's otherwise configured sources. Use a new output
directory. `--case TERM.QUERY.001` repeats for selected stable IDs, and
`--expected-core <full-SHA>` is required for later commits that change the core
baseline. Checkout identity and engine cleanliness are checked before and after
the suite. Each row records its test selector, Rust source, exact command and raw
log. A zero-test, skipped or unsuccessful Cargo run does not pass. The existing
independent-engine CI job still runs all engine tests and its boundary checker.

Literal probes use separate, hand-authored expectations. The historical resize
probe compares against a deliberately labeled historical Ember algorithm; it is
not an independent protocol oracle. The two parser profiles share the TUI stream
fixture but intentionally have different CSI storage limits. The Ember-only
query policy is not generalized to other profiles. These rows describe engine
behavior, not current frontend compatibility.

The frontend entries record immutable comparison revisions only. Aether,
Anvil, Ember, Forge and Frost are all `not-run` in this suite. This first lane
does not execute frontend PTYs, product services, GUI/IME, host transport or
performance. Add separate cases and platform-specific evidence before assigning
those lanes acceptance; a passing engine row must not promote them.
