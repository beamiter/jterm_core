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

## Product journey inventory

`product-cases.json` records 15 user journeys and six exact
comparison revisions. A source observation only points at a concrete code or
document anchor; it is not proof the user journey works. A partial observation
explains the missing relationship. An unestablished lane is an open work
target, not an assertion that a feature cannot exist. For example, a Vim PTY
smoke is not a native Vim9 GUI and fallback acceptance, and local preview
does not demonstrate remote-file handling. Forge and Anvil notebook routes
are observed; Ember, Frost and Aether are explicitly tracked as open on that
journey. The future native test IDs describe behavior still to be executed.

Run schema checks without any repository access:

    python3 scripts/terminal-family-product-catalog.py
    python3 scripts/test-terminal-family-product-catalog.py

To also confirm the catalog anchors, supply all six local Git checkouts
containing the listed immutable commits. Dirty working files are not read.
The verifier never fetches and never executes a terminal or a provider:

    python3 scripts/terminal-family-product-catalog.py \
      --repo jterm_core=. --repo aether=../aether --repo anvil=../anvil \
      --repo ember=../ember --repo forge=../forge --repo frost=../frost

Both modes report all five native lanes as `not-run`. The evidence fixture
and schema validation do not replace actual native tests.

For a development build of Aether, run its
`scripts/test-remote-project-webkit.py` with the built desktop and SSH helper
on both X11 and Wayland. Each successful run emits
`PRODUCT.REMOTE.001.NATIVE.001` with the display backend and both SHA-256
digests. The driver compares actual local and remote bytes, replaces a file
and a Workspace record while they are selected, and checks that stale
versions cannot open as the current source. The frozen comparison revisions
above keep their original results.
