# Architecture checkpoint packaging

The architecture candidate is packaged as a coordinated checkpoint across the
existing repositories. Frontend manifests pin exact core commits, not branch names.
This is development work, not a release or a completed shared terminal runtime.
The earlier milestone reports retain their original local-only validation scope.

## Source layout and dependency identity

- `jterm_core` remains the root package, version 0.2.0 and Rust 1.89 minimum.
  The SQLite security update requires this higher minimum; see
  [the dependency security note](../sqlite-security-2026-0459.md).
- `crates/jterm_engine` is a separately locked, independent Cargo workspace,
  version 0.1.0 and Rust 1.86 minimum. It uses smallvec, unicode-width and
  unicode-segmentation.
  It has no core, GUI, PTY, Agent, filesystem, network or executor dependency.
  The shared terminal, Ember and Frost use the same 8 MiB accounting for
  stored compressed scrollback rows in addition to their configured row limits.
  Aether and the GTK candidate search retained text with the engine's bounded
  literal search, preserving the search range across live output and resize.
- `crates/jterm_runtime` has a separate lock and a Rust 1.89 minimum. It owns
  scheduling, process observation, typed exit status, signalling and reaping.
  Aether and the private standalone GTK candidates enable the Linux PTY
  profile. It owns the terminal grid, queued logical input and its reply
  boundaries, and uses the shared pidfd-bound child lifecycle. Ember enables
  the session actor for its existing native PTY backend and legacy terminal owner.
  Frost also uses the actor for its ordinary terminal and Vim9 workbench.
  The actor owns the bounded input FIFO, resize, cancellation, output
  batching and native teardown. Frost keeps separate input and response
  byte allowances and holds the unreaped leader identity until its reader
  delivers the exit tail. Native input overload still retires the writer
  and blocks the final syscall.
- `crates/jterm_remote_client` owns the JWM helper protocol and process
  controller. Ember, Frost and Aether use it in their local builds. The
  private GTK builds use it through the core facade.
- `crates/jterm_protocol` defines the Aether terminal v3 compatibility
  protocol and the local desktop's v4 Channel payloads. The v4 snapshots
  and deltas retain screen, row wraps, grapheme text, underline types and
  colors, and referenced OSC 8 targets with exact u64 decimal frame counters
  and a delivery acknowledgment per stream.
- The core Workspace controllers drive CLI catalog organization, context,
  review, evidence search and read-only remote browsing in Ember and Frost.
  Forge and Anvil use the same conversation scope, evidence search and
  remote browser in their private builds.
- Ember and Frost pin both package names to the same complete Git commit in the
  existing `beamiter/jterm_core` repository. The family development runner
  can override these pins with matching sibling sources. Forge and Anvil
  pin only core for their ordinary release builds.
  No new repository or crates.io publication is required.
- Development pins must be updated only after the core/engine commit exists.
  Publish that exact core commit before frontends that depend on it. Never
  publish manifests containing the provisional `../jterm_engine` dependency.
- `scripts/family-cargo.py` provides explicit local overrides and private
  manifests/locks. It prefers the nested engine, supports older sibling layouts,
  and runs native tests from the original checkout. Overrides follow the Cargo
  subcommand so Clippy receives the same candidate as Cargo check/test.
  Aether's three release manifests use one published revision for the shared
  crates. Forge and Anvil pin their optional `jterm_gtk_view` dependency to
  the same revision as `jterm_core`. Build the candidate in either app with
  `cargo build --locked --features shared-terminal`. For local core changes, use
  `python3 scripts/family-cargo.py --forge-shared-terminal forge build` or
  `python3 scripts/family-cargo.py --anvil-shared-terminal anvil build`.

## Standalone checks

From this repository:

```sh
cargo test --locked --manifest-path crates/jterm_engine/Cargo.toml
cargo +1.86.0 test --locked --manifest-path crates/jterm_engine/Cargo.toml
cargo test --locked --all-features --manifest-path crates/jterm_runtime/Cargo.toml
cargo clippy --locked --manifest-path crates/jterm_engine/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path crates/jterm_engine/Cargo.toml -- --check
python3 crates/jterm_engine/scripts/check_boundary.py --engine-only
python3 scripts/test-family-engine.py
```

CI runs the engine separately on stable and Rust 1.86. The security audit covers
the locked graphs for root core, engine, runtime and the companion-host harness.
The full engine boundary checker also requires Ember and Frost checkouts beside
this repository; it verifies the actual frontend adapters and source identity.

## Source distribution

Cargo fetches both package names from the same immutable Git source. The runtime
path dependency follows that checkout; frontends do not add a separate pin. The engine
ships within that source tree, including its licenses, tests and separate lock.
Frontend materialization helpers therefore need no independent engine checkout.
Snapshot tooling includes `crates/` when present and excludes build products.
Existing archived snapshots remain historical evidence, not current build inputs.

When repinning Forge or Anvil, regenerate Flatpak Cargo sources using the pinned
generator and update the Nix full source-tree NAR hash. A gzip/tar checksum is not
a NAR hash. Manifest, dev-dependency, lock, policy and packaging identities must
agree; a local path override is not exact-publication validation.

## Compatibility and acceptance limits

The extraction preserves the Ember and Frost compatibility profiles and
existing host policy. On the local candidate, Frost's native Wayland IME
fixtures cover Block preedit and commit, multiline input, and an active Chat
foreground process. Ember's X11 PTY fixture covers device attributes and
status replies. Aether's native WebKitGTK/X11 fixture covers local Channel
delivery, split focus and restart, bounded search of retained history, and
read-only recovery of the same live PTY after a WebView reload. Aether schedules
local drain from the PTY reader wake and keeps a fallback poll for lifecycle
checks.
The native Wayland fixture covers preedit, a single commit to the focused
local PTY, and cancellation when switching between two live sessions.
Aether's local owner drains output and serves terminal replies before a
frontend receiver attaches. The SSH fixture covers reconnecting to the same
live PTY and recovering the final output of an exited PTY with input disabled.
The Workspace fixture verifies explicit launch from a persisted dependency
queue and acceptance against the inspected worktree. The `.jtnb.md` fixture
opens a local file, runs a shell cell in a dedicated working directory and
retains separate partial stdout and stderr when the user stops it.
The output review fixture exports Markdown and JSON from two frozen command
records after the original rows leave retained history.
The Aether project fixture uses a private OpenSSH server on X11 and native
Wayland. Same-name local and remote files retain separate bytes and identities.
Uploads preserve existing files. The read-only Workspace rejects a changed
host ID or evidence revision. The Vim9 X11 fixture covers the local project
tree, byte-column jumps, palette, quickfix search and build output. The jsh
fixture recalls an editable command and checks foreground-input isolation.
Native Forge and Anvil packages built with `shared-terminal` use the shared
runtime for eligible direct local sh and Bash tabs and splits configured
with `terminal_mode = "vte"`. The native tests leave the runtime override
unset for this route. The GTK tests cover exact PTY replies,
bracketed paste, scrollback reflow with a retained reading position, and
output drain in a hidden tab. The existing Find action opens search for the
focused shared pane. Native X11 tests cover copying matches across soft wraps,
searching older rows while the PTY writes, and independent searches in two tabs.
Native Wayland tests cover preedit cancellation
when switching tabs, one commit in the focused pane, and clipboard transfer.
The ordinary local jsh route also defaults to the shared runtime with
`terminal_mode = "vte"`. Native X11 acceptance checks Bash startup, separate
split identities, and a real app restart that restores jsh's session ID,
working directory and exported state. `JTERM_SHARED_RUNTIME=0` selects VTE.
The local Linux `--execute` route also uses the shared runtime in conventional
terminal mode. Named local tool tabs use the same launch path with their own
argv and working directory. The native probe in
`crates/jterm_gtk_view/tests/native_explicit.py` checks argument boundaries,
executable text, the exact query reply and keyboard input, reaping, and the
visible exec failure. Run it against each app built with `shared-terminal` on
X11 and on a private compositor with `--backend wayland` and a keyboard seat.
Linux native Forge and Anvil packages built with `shared-terminal` use the
shared runtime for both Vim9 displays by default. The native fixtures check
this default and save a real file with no `JTERM_SHARED_RUNTIME` override.
The embedded X11 fixture also checks the exit status delivered to the
waiting shared shell. `JTERM_SHARED_RUNTIME=0`, Flatpak and featureless builds
select the existing VTE editor backend. Shell integrations, remote sessions
and other terminal modes can also use VTE.

Forge and Anvil builds with `shared-terminal` use the GTK document view for
eligible completed Block and Chat commands and output. A bounded document
retains supported ANSI styles, grapheme clusters, OSC 8 targets and plain
HTTPS and HTTP links. The native tests cover fold-aware find, output filters,
cross-card clipboard and rebuilding cards after Undo. Captures outside the
document profile use VTE. `JTERM_SHARED_FINISHED_OUTPUT=0` selects VTE for
output. `JTERM_SHARED_FINISHED_COMMANDS=0` selects VTE for the entire card.
Native Linux packages built with `shared-terminal` default to the shared
runtime and live GTK terminal for new Block, Chat, and Unified panes.
`JTERM_SHARED_ACTIVE_TERMINAL=0` rolls back Block and Chat at launch.
`JTERM_SHARED_TRANSCRIPT=0` rolls back Unified at launch. Flatpak keeps its
existing host-bridge default. The native card fixture checks that each mode
keeps its session and terminal view through Find, an alternate-screen
application and the completion of consecutive commands.

Release acceptance still requires the complete shared session lifecycle,
Aether's remaining product journeys, the GTK production migration, and
cross-terminal platform, performance, upgrade and rollback evidence. Each
native result applies to the tested candidate binary and scenario.
