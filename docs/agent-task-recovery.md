# Authored task checkpoints

Anvil, Forge, Ember and Frost use `TaskManager::with_recovery` and the shared
`agent_task::recovery` implementation. Restart reconstructs inert task rows with
their original TaskId, goal, recipe and worktree. Opening the Tasks panel shows
them in the ordinary inspector. **Start/Open is still an explicit user action**
and uses the existing provider, sharing-policy and pinned-workspace preflight.
This starts a fresh provider run in the retained worktree; it does not reconnect
a dead process or reconstruct a provider conversation.

## Audit of the previous boundary

| Information | Previous lifetime | Checkpoint behavior |
| --- | --- | --- |
| TaskId | TaskManager memory; Ember creation journal registration marker | Original TaskId retained |
| Authored title, goal, validation recipe | Running task only | Explicit bounded allowlist |
| Worktree repository/path/branch/base | Git metadata; Ember creation journal | Retained and checked together with device/inode |
| Lifecycle and validation attempt/result | TaskManager memory | Historical status, attempt and exit facts retained |
| PTY IDs, native epochs, approvals, runtime ownership | Process lifetime | Never restored or persisted here |
| Captured terminal context, output, error/request bodies | Memory/provider-owned records | Excluded |
| Credentials and environment | Runtime/provider configuration | Never read or copied by the store |
| Hidden state | Running task | Retained; reopening hidden work never starts it |

The legacy creation journal is a separate transaction protocol, not a task
snapshot. Its files remain untouched. A pre-upgrade record cannot acquire a goal
that was never saved. Tasks created from captured failed commands also retain
that older behavior; this format deliberately does not serialize their captured
terminal evidence or turn it into a fictitious authored goal.

## Format and ownership

Each frontend uses its existing per-user `APP/agent-tasks` managed root and the
same `.task-checkpoints/tasks.json` version-1 schema. The store admits at most
128 tasks and 4 MiB. Unknown versions/fields, duplicate identities, malformed
JSON, unsafe permissions, symlinks and identity changes fail closed. The original
file is not deleted, quarantined or overwritten on load failure. Future schema
migrations must be explicit; the first task-checkpoint version does not reinterpret
legacy creation records as task checkpoints.

The directory is private (0700); snapshots and the persistent lock file are
private (0600). An exclusive nonblocking Unix flock is held for the manager's
lifetime. A second instance using that same managed root cannot take over or
save authored tasks. The four applications normally have different managed
roots, so they do not block one another. This is compatible storage and domain
behavior, not automatic transfer of tasks between applications. Close the
owning instance before reopening its saved tasks elsewhere.

Writes use the core atomic replacement implementation, including file and parent
directory synchronization. Exact previous bytes are checked before replacement;
an outside edit is not silently overwritten. Any uncertain publication poisons
the writer until it is reopened. Recognized secrets in raw instructions and the
serialized envelope cause saving to fail, rather than silently changing an
executable recipe. The shared secret recognizer is not an arbitrary-secret
classifier: do not place credentials in goals or validation commands.

## Recovery authority

A checkpoint records history, not live process evidence. Non-completed tasks
reopen as Created; hidden tasks remain hidden and explicitly restore to an inert
state. Completed tasks retain their historical completed state. The inspector's
recovery detail reports the previous lifecycle, validation status/attempt/exit
and Agent exit. Runtime kind, PTY bindings and approval streams are cleared.
A previous validation attempt becomes Inconclusive and keeps its monotonic
attempt number; a saved pass cannot authorize acceptance of changed work.

Fresh recovery verifies the original directory identity and Git worktree's
repository/branch association. Missing/replaced worktrees remain visible with
the original goal as failed, unassigned, read-only recovery rows; Start and
terminal fallback are unavailable. Their durable checkpoint remains unchanged.
Repair the workspace and restart to retry verification. Runtime preparation
continues to apply its own containment and pinned-directory checks before an
explicit Start/Open action.

Reopening the same unchanged checkpoint in one manager is idempotent; an ID or
worktree already attached to different state is rejected. Every explicit new
native run receives fresh runtime identities. No approval, terminal ID or old
provider epoch is replayed.

## Durability and UI behavior

An authored task's initial checkpoint is synchronized **before** registration
returns success, after worktree preparation. A failure leaves the worktree and
composer goal available for inspection/retry, rather than claiming the task was
saved. Ember's creation marker can lag this checkpoint after a crash: its
existing worktree-based registration deduplication finds the restored TaskId.

Hosts checkpoint subsequent summaries on their lifecycle ticks after reducing
runtime events. A kill between a state change and the next successful tick can
lose that latest summary, but not an acknowledged initial goal. Reopening/idle
polling does not overwrite historical facts with the inert projection. Unchanged
checkpoints do no filesystem/Git/serialization work on GUI ticks. Changed
checkpoints and startup verification currently perform bounded synchronous I/O;
this release makes no startup-latency or large-task-count performance claim.

Storage failures are logged and surfaced in the Tasks UI. On corruption or
another instance owning the store, authored registration fails instead of
silently falling back to memory-only persistence. There is no automatic
checkpoint eviction or worktree deletion at the capacity limit.

## Verification

The core recovery suite covers private permissions, symlinks, replacement of
worktree directories, secret rejection, duplicate startup/reopen, malformed and
future documents, duplicate JSON fields/identities, legacy creation-journal
preservation, incomplete temporary writes, killed-process lease release, original
goal/TaskId recovery, validation-authority invalidation, unavailable-worktree
inspection and explicit fresh native stream creation. A codec matrix uses all
four frontend names. This matrix does not substitute for native GUI testing.

### Validation on 2026-10-26

- Shared Agent domain: 276 tests passed, serially. The pre-existing
  `cgroup_scope_kills_term_ignoring_setsid_descendant_before_reap` host-dependent
  test was explicitly excluded, matching the repository's established suite.
  The 12 new recovery tests include a subprocess killed after a durable write.
- Strict shared-core all-target Clippy (`-D warnings`) passed.
- Ember and Frost: `cargo check --offline --locked --all-targets` passed against
  the modified sibling core. Frost retains two existing vendor deprecation warnings.
- Anvil and Forge: all-target checks were attempted but stopped in GTK/GLib
  dependency build scripts because `pkg-config` is absent. An attempted package
  index update failed on container user/group permissions. Application type checks,
  GTK native tests and real GUI restart validation are not claimed.
- Source formatting and whitespace checks passed. All four deterministic packaged
  core archives were refreshed from the same source and compared byte-for-byte.

These changes are submitted on a review branch rather than asserted to be a fully
validated five-frontend release.
