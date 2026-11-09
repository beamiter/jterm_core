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
