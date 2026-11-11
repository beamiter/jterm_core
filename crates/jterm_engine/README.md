# jterm_engine: provisional extraction

This independent crate, distributed at `jterm_core/crates/jterm_engine`, contains
production code extracted from Ember:
cell/style/color types, physical grid storage, stable row identity, compressed
scrollback, bounded search decoding, dirty regions, and an opaque hyperlink ID.
Ember re-exports these types and uses them in its existing terminal implementation.
Both Ember and Frost use the shared incremental parser with explicit compatible
profiles; neither frontend retains a second live byte scanner.
Frost shares cell/style/color/cursor types and uses one engine grid backing store
inside a thin wrapper retaining its identity revision and resize policy. Frost
keeps its own scrollback compression and streamed search. Ember also uses the shared VT mode and
keyboard protocol state, mouse reporting encoder, and GUI-neutral keyboard
encoders. Host shortcut routing, IME pairing, and text spoof filtering remain
in Ember; the engine does not acquire application policy.

This is **not a complete terminal engine**. ANSI interpretation, Unicode width,
reflow orchestration, selection, structural primary/alternate switching, command
metadata, rendering, and host integration remain in the applications. No xterm.js
or alternative terminal emulator was introduced.

Dependencies are `std` and `smallvec` only. This crate must not depend on GUI,
PTY, Agent, companion, jterm_core, global application identity, filesystem or
network services. Hyperlink IDs carry no URL-opening authority; validation and
interning remain in the host. Row ID allocation remains host-owned; zero means
untracked. Fixed-length mutable slices allow existing region moves without
exposing storage vectors or permitting callers to resize those vectors.

The storage algorithms and dimension assumptions are deliberately unchanged.
Hosts must clamp dimensions before grid construction and preserve wide-cell and
row-identity invariants during structural changes. The historical generic
scrollback decoder is not newly hardened by this extraction; bounded search
admission uses the existing checked decoder.

## Validation

`python3 crates/jterm_engine/scripts/check_boundary.py` checks source-level dependency
direction and production imports without requiring Rust. It does not prove Rust
compilation or runtime behavior.

Run `cargo test --locked --manifest-path crates/jterm_engine/Cargo.toml` from
the jterm_core repository,
then run Ember's existing terminal/fragmentation/projection tests and Frost's
parser tests. Tests cover compression fidelity, wide cells, hyperlink metadata,
row identity, bounded decode and differential control-string framing. Standalone
checks do not substitute for the frontend suites.

The crate has its own workspace, lockfile and Rust 1.86 minimum. It does not
join the core dependency graph. Git consumers select `jterm_engine` from the
same exact jterm_core repository revision as their `jterm_core` dependency;
no separate engine repository or sibling checkout is required. Local family
experiments use `scripts/family-cargo.py` with command-scoped overrides.

Run the boundary checker with `--engine-only` in an isolated core checkout.
The full checker additionally requires Ember and Frost beside jterm_core.
This development branch packages a partial architecture migration, not a
finished shared runtime, native visual acceptance or a release.

Source originated in beamiter/ember (d6d8637) and the matching framing code in
beamiter/frost (4c1196f). Original MIT/Apache-2.0 license texts are retained.

### Earlier primitive-slice validation, 2026-11-08

- Rust 1.99.1 stable: `cargo test --locked` passed all 18 tests (nine unit,
  nine integration); no doctests are defined.
- Rust 1.86.0: `cargo +1.86.0 test --locked` passed the same 18 tests.
- Stable `cargo fmt --check` and `cargo clippy --all-targets --locked -- -D warnings`
  passed. The manifest declares Rust 1.86 as its minimum version.
- Python source-boundary checks passed all three tests.

These results validate the standalone engine slice only. Ember/Frost full builds,
frontend regression suites, native desktop behavior and release packaging have
not been established by these checks. The engine lockfile records smallvec
1.16.5; the existing frontend lockfiles may resolve a different compatible version.

### Live screen slice

Ember uses ScreenState as sole live grid/cursor/pen owner. Row writes, erasure,
wide-cell edits, cursor saves/movement, margins and tabs run here. Host consumes
ordered mutation effects and still owns history, scrolling, resizing, graphics
and provenance. Stable/MSRV standalone slice now has 32 tests, including
independent pre-extraction row goldens; source boundary checks total four.

### Live incremental parser slice

Ember now owns exactly one `jterm_engine::parser::Parser`. It owns UTF-8 carry,
ESC/CSI syntax, OSC/DCS/APC framing, cancellation and overflow state; the former
host scanner and carry fields are removed. The host pulls and dispatches one
event before requesting the next, so same-feed RIS, command markers, Kitty
cursor changes and protocol replies retain wire order. The parser has no host,
GUI, filesystem, clock, graphics decoder or jterm_core dependency.

ANSI/OSC semantics, character width, command provenance, clipboard authorization
and graphics decoding remain host responsibilities. This is a live scanner
extraction, not completion of the entire VT interpreter. Completed OSC/DCS
payloads retain the historical uncapped direct-input behavior; only unfinished
carry is capped. Oversized OSC/DCS drop behavior remains batch-dependent, while
oversized APC is discarded through its terminator. Do not describe the parser
as globally bounded or every overflow case as fragmentation-invariant.

The diagnostic `snapshot()` allocates a compatibility view for tests; it is
not a shadow state owner or a production feed path.

Latest validation (2026-11-08): all 43 engine tests pass on stable 1.99.1 and
MSRV 1.86.0, formatting and warnings-denied Clippy pass, and all five Python
boundary checks pass. Ember's 463 terminal tests and 132 UI tests pass, as do
all-target checks and warnings-denied all-target Clippy. The terminal tests
include an independent pre-migration oracle with 916 input/partition cases,
each compared after its prefix and final feed. Capture provenance and the
compact expected fingerprints are in Ember's `tests/fixtures/parser/`.
These checks do not establish native desktop visual behavior or release packaging.

### Structural region slice

Engine executes one-row scroll and batched line edits while host applies history,
graphics, provenance and revision effects in original order. IL/DL retain their
legacy wrap-flag distinction. All 106 original production cases match exact
values; 52 engine tests pass stable/MSRV and six boundary checks pass. Ember
terminal:: has 464 passes plus one ignored fixture printer, and ui:: has 132
passes. Structural policy and resize are not yet engine-owned.

### Local semantic dispatcher slice

Ember now runs `semantics::dispatch_local` against its existing live ScreenState
and ProtocolState before host fallback. The engine implements local cursor/tab/
charset operations, saved cursor, rendition/extended colors, selective line erase,
character insert/delete/erase, keyboard protocol setters and cursor/status replies.
Typed ordered effects carry row mutations, provenance cursor movement, reply bytes,
bell and explicit background sidecar changes back to the host. Unhandled commands
return before any state mutation or effect; mode lists containing host hooks are
not split or reordered.

Host policy still owns structural orchestration, printable width/composition,
OSC/graphics/clipboard/command lifecycle, identity/theme replies, hard reset and mode
hooks. Existing permissive syntax handling, protected SGR resets, background
sidecar behavior and pending-wrap query/erase behavior are intentionally retained.
Frost does not consume this Ember-compatible semantic dispatcher yet.

The semantic fixture was captured on the original production host dispatcher
before integration: 3,160 cases/cuts with prefix and final snapshots. It is
separate from the unchanged 916-case parser oracle and 106-case structural oracle.
Seven standalone semantic tests include 4,000 deterministic adversarial SGR
comparisons against a frozen copy of the original implementation. Capture uses
exclusive file creation and cannot overwrite the committed expected data.

### Frost legacy parser and physical resize

Frost uses the versioned FrostLegacyV1 profile, preserving its original UTF-8,
CSI raw-parameter, ESC, feed-boundary overflow and APC recovery behavior. Ember
remains the default profile. Explicit feed boundaries and parser-generated safe
capture metadata retain Frost background-token timing without exposing private
control-string payloads. The two profiles deliberately retain documented legacy
differences; they are not claims of identical frontend semantics.

ScreenState now performs physical geometry changes and final-slot ID allocation.
Host still chooses archived rows and owns history/selection/provenance/graphics
notifications. Live resize is cropping, not historical viewport reflow.

Current aggregate: 72 engine tests pass stable/MSRV1.86 with Clippy; nine source
boundary checks pass. Independent Frost oracle covers 2,139 partition records,
6,958 observations and 91 lanes. Resize adds 305 production snapshots and a
10,125-case old-algorithm comparison. Native GUI/IME/release packaging remain
unvalidated.

Frost profile capture also preserves direct versus carried-string takeover-hook
positions. A verbatim original-scanner trace and an injected extra-hook mutant
verify this distinction; original expected fixtures remain unchanged.

### Immutable raw paint frames and publication state

`frame` provides validated immutable raw-cell snapshots with shared rows, complete-row exact-base deltas, and an epoch-bound atomic receiver. It preserves cell/style/hyperlink identifiers and pending-wrap as data; it does not normalize glyphs, resolve URLs/colors, own graphics, or implement history projection. Physical cursor coordinates remain in bounds. Full frames handle layout/screen changes; checked epochs/revisions prevent delayed data from silently replacing newer paint.

`publication` provides a pure per-subscriber acknowledged baseline, one in-flight frame and one coalesced latest frame, with exact acknowledgment/rejection and checked revision allocation. It contains no clock, executor or IPC. Hosts still must bound dimensions, byte allocation and subscriber count; cardinality alone is not a global memory budget. Rejecting a flight requires a newer desired publication before a full resync, never a blind resend of an old revision.

Frost's raw visible cache now stores the engine frame itself, and its existing widget consumes borrowed row slices. Scrolled/transformed views remain application materializations; their provenance and policy do not move into the engine. This is a native rendering boundary, not a Tauri GUI implementation or a complete shared terminal/application core.

### Alternate transitions and frame boundary

Engine owns actual alternate swaps and saved drawing-state operations; host
watch/graphics/selection effects remain outside. New immutable CellFrame, exact
row-delta receiver and pure subscriber publication state are renderer-neutral.
Frost's real raw render path now consumes these shared rows without a second
raw cache; projected history remains application-owned. Publisher state bounds
snapshots, but adapters must also enforce byte/subscriber limits.

Latest aggregate: 90 stable/MSRV tests, Clippy/fmt and 11 source boundaries pass.
This remains a partial architecture upgrade, not a Tauri GUI or complete shared
spawn/session runtime. See family validation documentation for native suite
results and independently reproduced environmental Git-trust failures.

### Ember soft reset and status strings

The live Ember dispatcher also owns DECSTR (`CSI ! p`, no private prefix) and
DECRQSS (`DCS $q`). DECSTR returns an ordered background effect and preserves
grids, cursor, alternate screen, unrelated modes, Kitty state and host sync
lifecycle. DECRQSS returns exact legacy reply bytes, including unknown selectors.
This semantic policy is Ember-specific: parser profiles do not select it, and
Frost must not call it as a substitute for its distinct soft reset policy.
RIS, mode hooks, device identity, OSC, graphics and clipboard remain host-owned.

### Ember per-parameter modes

`mode_mutation::apply_ember_mode` owns Ember's pure ANSI IRM and DEC origin,
wrap, cursor, keypad and recognized input/mouse mode mutations. It deliberately
preserves pending wrap on origin changes and distinguishes ANSI 4 from private
4. Unknown modes remain absent. The explicit namespace is supplied by Ember's
unchanged outer CSI dispatcher; this function never parses or reorders a list.

Focus (1004), alternate/save-restore (47/1047/1048/1049), synchronized output
(2026), and paste grants (5522) return unhandled with no changes in either
direction. The frontend performs these hooks in parameter order, retaining its
observer cadence and malformed-prefix/intermediate admission behavior. The
policy is Ember-specific; other terminals are not repinned by this extraction.

### Physical active-grid fills

`ScreenState::erase_screen` owns the ED2 physical fill: every active cell becomes
an unprotected, unlinked BCE blank with default foreground/style. It clears the
pending-wrap latch without moving the cursor or changing margins. DECALN's
`screen_alignment_test` fills default-style `E` cells (not BCE), resets margins,
homes the cursor and clears pending wrap while retaining origin mode. Neither
operation changes row identities/wrap metadata, drawing/saved state, the last
printed character, or the inactive grid. These are in-place edits, not output
ownership events or row replacements.

Ember invalidates provenance before calling either method and retains all
history, dirty/revision, cursor-reposition and graphics hooks in their original
order. ED2 also retains its host pre-invalidation latch clear for hook prestate;
the engine independently clears the latch for standalone callers. ED0/1/3,
selective erase, parser admission and alternate transitions are unchanged.
Other frontends are not repinned or switched to these methods by this change.

### Vertical-motion slice

`ScreenState::vertical_motion` owns physical LF/VT/FF/IND, RI and admitted
pending-autowrap movement, including real one-row region shifts. It returns
original departure coordinates and a `RegionMutation`; it is not a delayed
protocol queue. LF scrolls only at the exact bottom margin, RI clears pending
wrap even for invalid margins, and wrap marks the departed row and zeros the
column before scrolling. NEL uses `begin_next_line`, the host's immediate
reposition notification, and then LF.

Ember still admits output and chooses archive eligibility. It consumes identity,
Kitty, dirty and captured-history effects synchronously, then computes LF's next
boundary using the updated history counter or rebases an initial wrap. History,
provenance, revisions, viewport policy and allocator ownership remain host-owned.
The independent physical reference test covers valid and invalid margins,
original captured identities, allocator counts, full screen state and NEL's
pre-hook boundary. Ember additionally replays the immutable original-production
vertical corpus. This is a narrow milestone-2 extraction, not a completed engine
or a change to Frost's vertical semantics.

## Ember dynamic-color ownership slice

The `colors` module provides the live `ColorState` owner for Ember's OSC
4/104/10/11/12/110/111/112 state and replies. `TerminalDefaultColors` contains
host-supplied theme RGB defaults; foreground/background/cursor overrides and
all 256 palette overrides live in this owner, not in a host mirror. Renderers
borrow overrides and keep GUI conversion, opacity, bold/dim policy and theme
presentation in the host.

Color replies use the existing ordered `Effect::Reply`; the adapter delivers
them immediately at the original dispatch position and supplies each OSC's
BEL/ST terminator. Only a host theme update with mode 2031 subscribed and a
changed effective-background classification produces a color-scheme notice.
OSC 11/111 themselves do not broadcast it. The protocol owner retains mode
2031; the color owner does not duplicate that bit. RIS may clear overrides
while retaining theme defaults without notifications; DECSTR does not reset
this owner.

This is an Ember-compatible lane, not a migration of all four applications.
Clipboard authorization, graphics, command lifecycle, non-color OSC handling,
parser admission, rendering and host effects remain outside this module.
