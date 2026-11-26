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
  The shared terminal, Ember and Frost account for stored compressed
  scrollback bytes as well as rows. Their default byte allowance is 8 MiB.
  Ember sizes the allowance for larger explicit row limits through the shared
  policy, with a 32 MiB ceiling per terminal.
  Aether and the GTK candidate search retained text with the engine's bounded
  literal search, preserving the search range across live output and resize.
- `crates/jterm_runtime` has a separate lock and a Rust 1.89 minimum. It owns
  scheduling, process observation, typed exit status, signalling and reaping.
  Aether and the private standalone GTK candidates enable the Linux PTY
  profile. It owns the terminal grid, queued logical input and its reply
  boundaries, and uses the shared pidfd-bound child lifecycle. Ember enables
  the session actor for its existing native PTY backend and legacy terminal owner.
  Frost launches its ordinary terminal and Vim9 workbench through the shared
  native PTY startup and runs both with the session actor. Restore retries
  the launch in the default directory only after a working-directory error.
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
  For an isolated Aether worktree, run
  `python3 scripts/family-cargo.py --aether-checkout /path/to/worktree aether build`.

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
The full engine boundary checker verifies the Ember, Frost and Aether adapters
in neighboring checkouts. Use `--aether-checkout /path/to/worktree` for a
separate Aether candidate.

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
existing host policy. Frost applies local ANSI and DEC mode parameters through
the engine. Its adapter applies the mouse scroll lease, alternate-screen and
synchronized-output effects in parameter order. The engine also owns the
physical cursor motions and region shifts behind LF, IND, NEL, RI, soft wrap
and SU/SD. The adapter still admits history using its existing primary,
synchronized alternate-screen and inline CLI policy, then updates the visible
row and history revisions. Printable insert mode and CSI ICH use the same
Frost wide-cell edit. On the local candidate, Frost's native Wayland IME
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
cross-card clipboard and rebuilding cards after Undo. Screen captures and
shell reports can both supply document command headings. Review and retry use
the record's provenance. Anvil applies supported historical markup when the
projected text matches the retained command. Cross-card copy joins the logical
text of each document surface with one newline between surfaces. Finished
cards snapshot the live OSC 10 and 11 colors for the surfaces that use them.
Captures outside the document profile use VTE. `JTERM_SHARED_FINISHED_OUTPUT=0` selects
VTE for output. `JTERM_SHARED_FINISHED_COMMANDS=0` selects VTE for the entire card.
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

## November 23 common candidate

The [six-repository candidate](family-candidate-2026-11-23.json) verifies
seven production Cargo locks at `afb42aa`. The release checks for those exact
inputs include [core CI](https://github.com/beamiter/jterm_core/actions/runs/45075224137),
[Frost CI](https://github.com/beamiter/frost/actions/runs/45098897027), and
[Aether Linux contracts and build](https://github.com/beamiter/aether/actions/runs/45133063485).

[Forge](https://github.com/beamiter/forge/actions/runs/45098531795) and
[Anvil](https://github.com/beamiter/anvil/actions/runs/45098228752) reached
the older result in shared GTK history search and then lost its X11 selection.
The shared view now preserves PRIMARY ownership while it replaces the result.
The native test verifies the older result's selection and primary clipboard.
Local Forge and Anvil builds passed the X11 ordinary-pane suite with this fix,
including the older history match, clipboard, Vim and hidden PTY probes.

The shared runtime admits per-child SIGCHLD watchers and rejects dispositions
that automatically reap children. Aether's candidate builds exercise the same
local PTYs through WebKitGTK, including renderer reload and Wayland composition.
Its SSH fixtures use a temporary OpenSSH config on the hosted runner. The
latest [native workflow before this fixture change](https://github.com/beamiter/aether/actions/runs/45125299673)
failed at user namespace setup for all three SSH scenarios.
The candidate passed the local X11 and Wayland remote project cases, the
interactive SSH relay recovery case, and the Debian install and Workspace
backup rollback rehearsal. The local builds use path overrides for the core.

Ember's [CI at `97702a7`](https://github.com/beamiter/ember/actions/runs/45075318214)
passes Rustfmt and the native frame receipt gate. The two X11 Block Search
scenarios in that run fail after their oldest rows leave the 8 MiB history.
The raw search cannot seal its command range. The snapshot has the early
match text and loses the original line needed for navigation. A replay of
the exact output rows, including the 256-row flush, retains 49,933 rows with
the default byte allowance. The local Ember candidate applies the existing
160,000-row fixture setting in the shared byte policy. The raw replay keeps
all 100,256 rows in 16,842,712 bytes and the snapshot replay keeps all
128,256 rows in 21,546,712 bytes.

The local release build passed both original X11 Block Search scenarios.
The 100,000-row run exercised loading, Stop, Restart and completion, then
verified the early match and its adjacent unqueried row in the terminal.
The 64-command run used Earlier history and verified the earliest snapshot's
match and neighboring row after the picker closed.

The Frost candidate uses the shared `ScreenState` in its live terminal. The
same release build passed native Normal, Block and Chat input on X11 and
Wayland, Wayland Block-mode IME composition and commit, and the native X11
resize fixture. A real alternate-screen PTY processed a further 1,380,000
bytes while a second tab held input focus and returned the expected cursor
report. Switching back restored the alternate image, and leaving it restored
the primary image and shell identity. The candidate adds this scenario to
Frost's X11 CI through `scripts/native-background-screen-smoke.py`.
The X11 PageUp and tab-return fixture also verified the first complete repaint
after reattachment to the same pane.

Ember's local candidate uses a shared `SessionHandle` for both output pumps.
The runtime keeps any unapplied tail across budget and response-gate changes,
and delivers the exit status once the parser returns. The X11 and Wayland CI
workflows include a real PTY scenario for each pump. It checks exact cursor
replies after 1,365,000 bytes of output, the clipboard effect from the final
batch, and keyboard input on the surviving tab's original shell.
The same locally installed release binary passed both desktop scenarios and
both X11 Block Search cases. In a separate X11 startup failure, it retained the
last terminal line and displayed exit code 42. The isolated installer rehearsal
preserved the customized configuration byte for byte through upgrade and
rollback.
