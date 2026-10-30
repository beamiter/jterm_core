# Eva host adapter contract

`companion_host` is shared window-level glue, not a new personality, audio stack,
HTTP provider, database, PTY writer or command executor. Dependencies are one-way:
protocol/core/client/atlas → jterm_core adapter → native frontends. The atlas is
re-exported; there are no copied skins in each application.

Create one `SharedHost` per GUI window, never per pane/history row/preview. A
persisted opt-in mode replaces legacy rendering for that window; do not create a
legacy fallback if the daemon is absent. `EVA_COMPANION_MODE=shared` is a launch
override; each frontend also offers a persisted setting. Reconnect preserves the
old possible-body token and remains hidden until a new valid grant.

## Rendering lifecycle

1. `begin_frame(serial)` before polling/painting. `poll` updates the gate and
   synchronously removes drawing authority on revoke/disconnect.
2. Draw only when `gate.token()` is present, using current safe geometry. Keep
   token/boot/incarnation/epoch with any delayed callback; `accepts_callback`
   rejects stale targets. Resolved block IDs must never be rebound by position.
3. `body_submitted(serial)` only after actual body/compact commands are in that
   frame's display list. A grant is not a presentation acknowledgement.
4. `frame_presented(serial)` only from a verified successful native presentation
   boundary, not message receipt, layout, heartbeat, animation tick or timeout.
   The matching proof cannot precede the revoke boundary. Duplicate/stale old
   grants cannot revive retired bodies. No-capability hosts never claim proof.

NativePresented is a native submitted-buffer boundary, not proof of arbitrary
compositor scanout or physical monitor visibility. Frozen/unverifiable surfaces
must leave transfer blocked. JWM's stronger compositor capability is separate.

## Transport and UI controls

Socket work runs on a named background thread. UI calls are bounded, nonblocking
channel/memory operations. Latest focus/geometry/input can coalesce; ordered
retirement, task, approval and user controls do not silently overwrite each
other. Queue failure suppresses residence. Only actual IPC changes wake the UI;
a disconnected hidden body does not request continuous animation frames.

A genuine session Rejected closes/reconnects transport; ordinary OperationError
is nonfatal and available in the card. On changed `connection_generation`, resend
current metadata with fresh geometry revision. Register ContextObserved before
context-bearing operations. Reply queues retain exact origin context/turn.
Never paste a dialogue answer into a new pane or the shell. ContextAccessReview
is not permission: show exact bounded selected text, require a separate deliberate
approve action, then attach one-use evidence to the intended turn. Memory review
must display exact scope/change/digest before approval; generated proposals are
never approval. The adapter never invents project identity from cwd.

## Validation

`cargo test --manifest-path tools/companion-host-harness/Cargo.toml` compiles this
exact production source with only its direct dependencies. It covers stale
callbacks/incarnations/surfaces, genuine retirement ordering, reconnect across
boot, duplicate grants, no-capability fail-closed, presented-vs-received,
Rejected-vs-OperationError and metadata coalescing. These pure tests do not prove
native pixel uniqueness, audio devices, display-server or hardware performance.

## Published source binding

This release pins protocol/core/client/atlas to public `beamiter/jcompanion`
commit `9aa569540091524d637a7115b64c587b9177a55b` (protocol version 1).
Cargo fetches these immutable sources; a sibling companion/atlas checkout is not
needed. Keep Cargo.lock and normal/test fixture manifests aligned. To verify:

```sh
cargo check --locked --all-targets
cargo test --manifest-path tools/companion-host-harness/Cargo.toml --locked
```

The focused harness compiles the actual production adapter source; it does not
prove real display/IPC/hardware behavior or replace whole-library regression tests.

Final public-pin verification: adapter **16/16 tests**, strict harness all-target Clippy, and production `cargo check --locked --all-targets` pass. An isolated export with no sibling source directories resolves both production and harness metadata to the exact published companion revision. Whole-library historical failures are separate from these scoped checks. Evidence: `docs/evidence/eva-public-pin-*.txt`.
