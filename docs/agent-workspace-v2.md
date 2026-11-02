# Agent Workspace v2

This release extends the existing hosted CLI, API and native runtimes. Hosted
execution already existed before v2. A GUI view detaches from that execution;
the host and provider lifecycle is separate. A host crash still produces an
interrupted or unconfirmed outcome, and surviving kernel leases prevent a
second writer. No new catalog, query, result or queue operation grants runtime
control or replays a provider request.

## Deployment

Use **workspace protocol 2** and **SQLite schema 5** as one release across the
shared core, the embedded host and Anvil, Forge, Ember and Frost. A protocol
mismatch is an explicit error. The client does not kill an older host, steal
its ownership or retry an uncertain operation to make an upgrade succeed.
Finish or stop active work with the currently installed build before a planned
host restart. Closing a GUI is not a host restart.

The new host upgrades an older supported database through the single writer.
An older binary must refuse the new schema. Back up the private state directory
before a planned downgrade; a schema rollback is not supplied. No protocol
version implies that SSH or DLC execution adapters are installed.

## One presentation and query contract

`WorkspaceRow::presentation()` separates runtime, task lifecycle, validation,
recorded history, attention and provider capabilities. Source-qualified
`ConversationId` values are stable across sorting and filtering. A displayed
row or a suggested primary action does not authorize approval, submission,
start, stop, resume, acceptance or owner takeover. Primary actions navigate to
the exact conversation and its evidence.

The authoritative inputs are current host catalog rows and
`AgentRuntimeManager::workspace_row(task_id)`. A frontend may construct a local
fallback with `WorkspaceRow::from_task(namespace, task, activity)`, supplying
only activity observed from actual owners. Saved lifecycle text is not proof
that a process is live. `IndexSessions` strips all runtime authority and retains
recorded metadata; a scanner cannot manufacture live requests or capabilities.

`WorkspaceQuery` is shared by catalog filtering and document search. It accepts
quoted phrases, negative terms and fields for provider, kind, project, branch,
status, runtime, validation, attention, tags and identities. `body:` and
`content:` require indexed text. `matches_document` lets a positive unfielded
term match row metadata or document text; a negative term excludes matches in
either. Quoted unknown fields, URLs and Windows paths remain literal text.
Invalid structured syntax is reported, rather than silently broadening a
search. Queries are limited to 4 KiB and 64 terms.

For CLI history, `status:` describes recorded provider history. `is:live` and
runtime fields describe observed execution. Reading an old input request or
approval does not create a live request. Archived records stay excluded unless
requested explicitly. `WorkspaceSearchIndex` orders the catalog projection by
relevance, recent, oldest, pinned-first or attention; frontends retain selection
by ID. The persistent body-search API uses its document cursor and does not
claim those catalog sort modes.

`WorkspaceAction` names actions consistently and records input scope. Composer
Ctrl+Enter and terminal Ctrl+] retain their established meanings. Navigation
shortcuts must not consume terminal input or IME composition. These contracts
are not a substitute for desktop IME or accessibility verification.

## Search and exact evidence

Schema 5 adds an incremental FTS5 trigram index and a bounded fallback for short
terms. `CatalogPage` pages up to 256 rows using a stable numeric cursor.
`Search` returns at most 100 hits, scans at most 4096 candidate records per
page, and reports coverage and pending work. Background backfill processes up
to 64 existing records per host iteration. Provider history remains explicitly
indexed excerpts; coverage does not claim every source file has been read.

Search hits carry conversation identity, event identity and revision.
`ReadSearchDocument` reads that exact revision and refuses a stale anchor.
Opening or searching evidence does not start a provider, send a message or
expand a model's context window. External text imports are bounded to 32
documents per batch and 256 KiB per document. Failed or partial imports must
remain visible as coverage limits.

## Context and handoff

`ReadContext` and revision-checked `SaveContext` persist a goal, completed work,
remaining work, constraints and selected materials. A packet holds at most
16 materials and 64 KiB of visible text. Materials can retain exact source
conversation, event and revision references. Reading history does not add it
to a packet automatically.

`BuildHandoff` produces a previewable draft for a **new** target conversation.
It does not transfer provider thread IDs, credentials, control leases,
approvals or runnable tool calls. The draft must fit the existing 16 KiB task
goal limit; an oversized draft fails without modifying its saved materials.
Starting the target task or sending the draft still requires the existing
explicit user action and its normal preflight.

## Result and validation evidence

`InspectResult` performs asynchronous, read-only inspection. Its report ties
task identity and run identity to the task baseline, current changes and
validation evidence. It does not run tests or accept a task. All changes from
the task baseline are displayed; pre-existing and external edits are not
attributed to the Agent.

The worktree capture uses SHA-256 over the verified task HEAD, baseline,
branch, status, tracked changes, staged changes and non-ignored untracked file
contents. Each captured Git output is bounded to 1 MiB. Untracked evidence is
limited to 2048 files and 16 MiB in total; unsupported paths, special files or
changes during capture fail explicitly. Ignored build artifacts are excluded.
Repositories containing submodules are currently refused for validation
evidence; the fingerprint does not claim to cover their nested working trees.
The fingerprint is a point-in-time observation, not a filesystem lock. Display
excerpts retain at most 128 KiB of diff and 16 KiB of status, with explicit
truncation fields. Truncating the display does not narrow the captured evidence
used for the fingerprint.

Validation captures evidence before and after the command, retaining its
command, cwd, shell and execution times. A passing command with changed
evidence is stale. Acceptance rechecks the current fingerprint and exact
validation command, and retains the existing ownership and lifecycle gates.
Passing validation and accepting a task remain separate operations.

## Task queue and observed resource use

`QueueUpdate` records bounded dependency intent in the same single-writer
SQLite database. A global revision prevents one view from overwriting another
view's changes. There are at most 256 entries and 32 dependencies per task.
The store requires exact catalog task identities, rejects self-dependencies,
duplicate dependencies and cycles, and applies updates atomically.

`QueueSnapshot::project(rows)` is read-only. Dependencies are satisfied only by
explicit task acceptance, including an accepted task subsequently archived.
A passing validation does not release dependent work. Missing task details,
unconfirmed outcomes and existing runtime owners stay visible. A ready task
must still be opened and started explicitly through its original host control
path. Queue writes never spawn a provider or schedule an automatic retry.

Observed overlapping worktree paths block readiness hints. This is separate
from the existing single-conversation writer lease. Catalog observations do
not lock a worktree against every external editor or shell, and incomplete
catalog coverage cannot prove the absence of other writers. Execution and
acceptance still enforce their own live ownership and evidence checks.

`WorkspaceOverview::from_rows(rows, catalog_complete)` counts current observed
CLI/API/native activity, task terminals, validation, acceptance and attention.
It labels an incomplete catalog explicitly and never infers process activity
from transcript text. Validation-only tasks and terminal task fallbacks are
not reported as native Agent execution. These are conversation/runtime counts,
not measurements of OS CPU, memory, wall-clock cost or billable usage.

Execution location intent supports Local, SSH and DLC. The current Agent
Workspace runtime has a local execution adapter. Remote intent is retained
visibly and is blocked with its missing-adapter reason. It is not silently
launched locally, and this release does not claim verified SSH/DLC Agent
execution or cross-host migration.

## Read-only remote workspace over SSH

`WorkspaceSshHost::new(alias, remote_executable, expected_host_id)` and
`WorkspaceClient::with_ssh_host(config)` provide a real read-only SSH transport.
The user explicitly supplies an existing SSH configuration alias, the absolute
path to the installed remote frontend or `jterm-sessiond`, and its host ID.
Run that program with `--jterm-workspace-host-id` on the remote machine to obtain
the ID. Executable paths containing spaces, parent traversal or shell syntax
are refused. `with_ssh_executable` supports an explicit alternate local SSH
binary for packaging or fixture transports.

The connection uses BatchMode and StrictHostKeyChecking, clears forwarding,
disables agent/X11 forwarding and local SSH commands, and invokes only the
fixed `--jterm-command-stdio` mode. The remote helper sends its protocol and
host identity before receiving any query. The client verifies both, then
sends one bounded JSON command on stdin. Requests and replies have an exact
request identity. Query text never enters the remote shell command line.
Source-qualified conversation IDs are returned unchanged; frontends keep this
catalog in a separate remote view and never activate it through a local runtime.

Both client and helper allow only `Refresh`, `CatalogPage`, `Search`,
`ReadSearchDocument`, `ReadContext`, `InspectResult` and `QueueSnapshot`.
Mutation and provider control are rejected before the local SSH process is
started and again at the remote helper. The helper connects only to an
already-running same-user remote host; it creates no host or database. SSH
known-host verification is still required independently of the workspace ID.

One supervised SSH process serves one request, with nonblocking bounded pipes
and a 75-second deadline. A wrong host, lost connection, malformed reply or
timeout returns an explicit error and is never retried automatically. Remote
observation does not enable SSH Agent execution, remote approvals, queue
mutation, DLC execution or cross-host task migration.

## Provider-reported usage

Native view snapshots optionally include `AgentUsage`. Codex's audited
`thread/tokenUsage/updated` cumulative thread totals replace earlier totals;
they are not added together. Claude result records accumulate once per
observed print turn within the native run, after checking the exact provider
session identity. Duplicate result records cannot add a turn twice. The
retained report identity set is bounded; exhausted coverage is marked partial.

Counts, source and scope remain explicit. Missing values mean unavailable,
not zero. Claude's provider-reported USD values use checked decimal addition;
Codex token events do not supply a cost. No token estimate, price table or
subscription-to-API billing conversion is used. Usage reporting has no effect
on runtime authority, task acceptance or the dependency queue.

Codex native supports structured approvals and verified native restore under
its existing pinned protocol. Claude print supports sequential turns but has
no interactive approval channel and no new stopped-native restore path; its
explicit terminal handoff remains the continuation route. Kimi remains
one-shot. The capabilities projection preserves these differences.

## Verification boundaries

Core tests use bounded in-memory data, temporary SQLite databases, fixture Git
worktrees and fake provider processes. They exercise identity checks,
historical/current separation, queue revisions and cycles, usage correlation,
search anchors, evidence fingerprints and existing host ownership. They do not
read real provider threads, credentials or invoke real models. SSH transport
tests use a local executable stub and an isolated host to exercise the actual
stdio bridge, identity mismatch and disconnect paths; these are not a claim of
testing the user's real SSH infrastructure.

Desktop compilation, visual behavior, X11/Wayland, keyboard accessibility and
IME interaction require separate frontend evidence. This document does not
claim those environments were exercised by core unit tests. The release
validation record must list the commands actually run and any remaining
desktop coverage limits.
