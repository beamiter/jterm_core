# Self-owned terminal engine: incremental migration

This work follows the revised architecture: Rust owns terminal semantics and a
future Tauri frontend draws Rust-produced views with a custom renderer. There is
no xterm.js dependency or alternative VT parser in the WebView.

## Baseline and ownership

`family-baseline.json` records the inspected source revisions, not a claim that
new candidates are releasable together. Preserve each consumer's release pin
until the shared candidate is published and the exact consumer revision passes.

| Component | Retained responsibility |
| --- | --- |
| jterm_engine | Extracted pure terminal primitives; eventually incremental VT state, modes, input encoding and ordered effects |
| jterm_core | Runtime policy, process lifecycle, application records, Agent Workspace, persistence, Vim9 |
| Ember/Frost adapters | Current live parser and business integration until each extracted lane is verified |
| Forge/Anvil adapters | Existing GTK/VTE path until a shared-view GTK widget is separately validated |
| Frontend renderer | Paint, platform input, IME placement, focus and UI-local caches |

Do not make jterm_engine depend on jterm_core: that would pull Agent, companion,
storage and runtime dependencies into the bottom layer. Moving a type is not the
same as moving ownership of the state machine.

## Initial candidate

1. Introduce a shared scheduling-only `runtime_wake::WakeSink`.
2. Ember's shell I/O workers receive that handle, not an egui context. Only the
   frontend adapter knows how to call `request_repaint`. Queue capacity, blocking
   send behavior, exit-tail ordering and single-child-reaper behavior stay as-is.
3. Forge wraps its existing coalesced eventfd notifier in the same handle. The
   descriptor-pressure polling fallback remains timer-driven with a no-op sink.
   This is a wake boundary, not extraction of the complete GTK-owned PTY class.
4. Extract existing pure terminal primitives incrementally into jterm_engine and
   re-export into production adapters. Retain actual producer/parser tests; a
   new parallel toy parser cannot establish behavior parity.

Wake callbacks must be nonblocking, thread-safe and non-panicking. Publish data
before waking and do not hold queue locks while calling the sink. Wakes may
coalesce; payloads may not. The sink does not own cancellation or a process.

## Initial host-context seam (Ember)

`TerminalState::new_with_host_context` accepts an owned hostname and trusted
notification application name. The existing `new` constructor resolves local
host inputs once at construction; OSC 7 and OSC 9/777 parsing no longer reads
the environment, filesystem or global application identity. RIS carries the
same context into the reset state. Explicit context tests exercise two hosts,
local/remote OSC 7 behavior, title fallback, notification sanitization and caps.

This remains a seam in Ember's application-owned composite state, not relocation
of the full live terminal state into the engine. Clipboard policy and command
bookkeeping still belong to the application layer.

## Initial synchronized-output clock seam (Ember)

`SyncOutputClock` is constructor-supplied and used only for DEC 2026 start and
watchdog expiry. The existing constructors supply the real monotonic clock.
The explicit combined constructor accepts host context and clock; RIS preserves
both while clearing synchronized-output state. The timeout remains strictly
more than one second; backwards test readings conservatively count as zero.

This is not a universal terminal clock. Clipboard authorization still expires
against real time with its existing `>=` boundary. Command timestamps, command
durations and search deadlines are unchanged. No-sleep tests cover restart,
explicit cancellation, idle expiry, resize, reset, fragmentation, independent
instances, snapshot/dirty-row effects and separation from clipboard expiry.

## Live protocol state and input encoding (Ember)

`ProtocolState` is the single live owner of Ember mode bits, active/inactive
Kitty keyboard flags and bounded stacks, and global xterm keyboard settings.
Parser mutations, queries, reset and screen swaps use it directly. The engine
preserves Ember's all-u16 flags and 32-entry stack; the narrower existing GTK
keyboard policy in core remains distinct rather than being silently substituted.

Mouse encoding and GUI-neutral logical key/modifier encoding are engine
operations used by Ember. Platform command aliases, shortcut routing, page
ownership, IME key/text pairing and text spoof filtering stay at the GUI edge.
Production fragmentation snapshots retain separately named protocol lanes.

This completes that narrow protocol/input slice, not full VT parsing, live
screen/history ownership or a cross-frontend renderer.

## Live screen operations and shared Frost cells

Ember stores its only live active/hidden grids and drawing state in ScreenState.
Engine row writes, wide edits, erasure and cursor operations return immediate
row effects with identity and distinct changed/printed spans. Host retains
structural scrolling/history/resize, graphics, row allocation and provenance.
Pre-extraction row goldens establish compatibility independently of new code.

Frost now reuses cell/style/color/cursor and opaque hyperlink types and a single
engine grid backing store, retaining its row identity revision, scrollback format, streaming
search and richer keyboard policies. These differences need explicit adapters
before storage or parser convergence; the migration is not a wholesale swap.

## Incremental parser ownership

Ember pulls one event at a time from the sole engine Parser and immediately
applies existing semantic handlers. UTF-8/ESC/CSI/string carry exists only in
engine. Independent original-scanner fingerprints cover 916 partition cases
across 69 lanes; references and capture provenance are kept with tests.
Semantic dispatch, width, graphics, clipboard and command ownership remain
host responsibilities. Completed direct OSC/DCS payloads retain existing cap
exceptions and oversized pending strings retain batch-dependent recovery.

## Acceptance gates

- Build and run jterm_engine tests without a GUI/display, PTY or Agent dependency.
- Run Ember production fragmentation, grid, scrollback, projection, selection and
  reflow tests after import changes. Compare bytewise and arbitrary chunk splits.
- Run Frost production parser fragmentation/cancellation tests after replacing
  any duplicated parser primitive. Differences in full engines remain explicit.
- Run core WakeSink tests and Ember shell event-publication tests.
- Run Forge PTY tests covering eventfd coalescing, shutdown, child reaping,
  descriptor-pressure fallback and tail output ordering.
- Use the family Cargo runner for local overrides, then separately validate
  released pins/locks after an explicitly authorized publication.
- GUI smoke tests still require the native dependencies and a real display.
  Vim/tmux, Chinese IME, pane switches during composition, mass output, resize,
  paste/backpressure and mode changes cannot be certified by pure unit tests.

Suggested local commands (dependencies must already be cached; runner is offline):

```
python scripts/family-cargo.py jterm_engine test
python scripts/family-cargo.py jterm_core test --lib runtime_wake
python scripts/family-cargo.py ember test --lib terminal
python scripts/family-cargo.py ember test --lib shell
python scripts/family-cargo.py frost test --bin frost
python scripts/family-cargo.py forge test --lib pty
```

Check the actual targets available in each manifest before extending filters.
The runner uses private manifests/locks and requires sibling jagent for runtime
consumers. It must also mirror the engine sibling for consumers with path deps.

## Remaining milestones

1. Inject host identity and monotonic time; separate application command records
   and policy effects from live VT state.
2. Move live parser/grid/history/modes/input encoding under one engine authority;
   switch Ember production ownership and prove unchanged adapter behavior.
3. Resolve Frost differences with shared corpus and migrate its production path.
4. Add Tauri custom Canvas renderer: Rust controls cell spans, snapshots and
   deltas. Verify epochs, revision bases, bounded in-flight frames and resync.
5. Bind normalized key/text/paste/mouse/IME events to current view/focus identity.
6. Integrate existing Block/Chat, Workspace and Vim9 authority without duplication.
7. Replace GTK/VTE terminal areas with a common GTK renderer only after parity
   and real display tests. Anvil remains unchanged in this initial candidate.

Do not describe steps 2–7 as complete merely because initial primitives compile.

## Ember reset/query ownership

`semantics::dispatch_local` now owns Ember DECSTR and DECRQSS, with ordered
`GlobalBackground` and `Reply` effects applied before the next parser event.
Only no-prefix CSI with exactly `!` and final `p`, and DCS beginning exactly
`$q`, enter these handlers; other events retain transactional fallback. Legacy
DECSTR parameters remain ignored. The Ember policy retains cursor/grids/alternate
state, unrelated modes, Kitty state and host synchronized-output lifecycle.
Frost's distinct soft reset is not migrated and parser profile choice does not
make this dispatcher safe for Frost. RIS and host policy remain in the adapter.
Independent pre-extraction production snapshots remain immutable, including
per-partition search revisions; engine differential tests supplement them.

## Ember mode-query ownership

The same Ember-compatible dispatcher now handles DECRQM: final `p`, exactly `$`
as intermediate, and no prefix or `?`. It reads the original first/default-zero
parameter and emits the existing ordered `Reply` effect. Origin/insert booleans,
permanently-reset ANSI 20, and the legacy private-mode recognition table remain
unchanged. Other prefixes/intermediates/finals remain transactional fallback.
Frost policy is not selected implicitly, and no other consumer needs a repin.

Before extraction, Ember captured 67,944 actual production per-feed snapshots
from 75 inputs and four rich primary/alternate, sync-active/inactive seeds. The
immutable lossless golden, exact raw-byte hash, original lookup bodies and source
provenance are retained in its tests. Engine tests independently compare every
u16 mode and mutation controls exercise real candidate source. Host lifecycle,
clipboard authorization, graphics, RIS and mode effects stay with the adapter.
The unchanged original reset/query fixtures remain part of the acceptance gates.

## Ember dynamic-color ownership

The next narrow Ember lane is `colors::ColorState`: theme RGB defaults,
foreground/background/cursor overrides, and the full 256-entry dynamic palette
have one live owner. It implements the existing OSC 4/104/10/11/12/110/111/112
behavior, color parsing, palette fallback and effective-background classification.
The host pushes `TerminalDefaultColors`, borrows owner overrides for rendering,
and does not retain a second mutable terminal-color mirror. Host GUI theme
objects and application-wide theme configuration remain host concerns.

OSC 4 pairs execute in input order, including queries between mutations.
Replies reuse `semantics::Effect::Reply` and are appended at the original
dispatch point before subsequent parser events. BEL/ST is supplied for each
OSC, not cached again inside the color owner. The existing host OSC terminator
also serves non-color protocols and remains in the adapter.

Mode 2031 remains owned by `ProtocolState`. A host theme update supplies the
current subscription bit and emits a notice only when the effective background
changes dark/light classification. OSC 11 sets and OSC 111 resets do not emit
that notice, nor does merely enabling 2031. DSR 996 reads current classification.
RIS clears dynamic overrides while preserving theme defaults without invoking
theme-update notification; DECSTR and primary/alternate switching do not clear
color state.

This lane is Ember-only. Frost policy is not implicitly changed, and Forge and
Anvil are not claimed migrated. Clipboard authorization, graphics, command
metadata/lifecycle, non-color OSC behavior, renderer presentation and other host
effects remain adapter-owned. Standalone unit tests supplement, not replace,
Ember's immutable pre-extraction per-feed full-state golden and frontend tests.

### Ember title-owner candidate boundary

`jterm_engine::titles::TitleState` is the sole candidate owner for Ember's
`window_title`, `icon_title` and `title_stack`, preserving their string/optional
pair values and Debug representation. OSC 0/1/2 setters accept host-sanitized
text and retain the 200-scalar cap. XTWINOPS 22 saves up to 16 entries (oldest
entry evicted); 23 always pops before selective restoration. Target 1 is icon,
2 is window, every other target selects both. XTWINOPS 20/21 replies retain
fixed ST and the original control/bidi filter via ordered `Effect::Reply`.

The host retains the `review_input` visual-spoofing sanitizer before admission,
parser framing, effect delivery, title display and window geometry. Operations
11/13/14/16/18/19 are intentionally not implemented by this owner. No reverse
engine dependency on `jterm_core`, GUI or host security policy is introduced.

Rust 1.86 and 1.89 each pass 118 engine tests, including six title tests, with
strict all-target Clippy and formatting checks. Original-production baseline
sealing and the consumer adapter remain separate acceptance requirements;
this core candidate alone does not establish Ember end-to-end parity or migrate
Frost, Forge or Anvil.

### Ember ASCII-run step candidate

The engine's `AsciiWriteStep` exposes one live operation boundary, not an effect
queue or another cursor owner. `step_ascii_write` checks empty input first, then
uses the existing pending-wrap and ASCII row writer without modifying either
primitive. The host completes each `NeedsWrap` through its existing vertical
motion/history/rebase chain; after `Written` it applies the exact `RowMutation`
before consuming more input. Autowrap is one run-level read of the protocol owner.

No row batching is replaced by per-character output and no parser takeover
observations are inserted between row steps. Only printable ASCII under ASCII
charset without insert mode uses the seam. Unicode/combining, translated charset,
insert and REP keep their original paths. Nonempty DECAWM-off admission still
clears a pending latch; empty admission leaves it untouched. The original
no-progress cursor behavior is retained, not silently clamped or generalized to
zero-sized grids. Differential core tests compare the explicit original
orchestration at every immediate host wrap/row checkpoint; they supplement, not
replace, immutable original-production consumer evidence. Other terminals are
not claimed migrated by this candidate.
