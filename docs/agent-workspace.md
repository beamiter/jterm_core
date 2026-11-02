# Agent Workspace

Agent Workspace is the shared conversation layer used by Anvil, Forge, Ember
and Frost. The desktop has a permanent conversation sidebar and one central
work surface. CLI conversations open in a real terminal; API chats and native
tasks use their own central views. The inspector is secondary. Inspecting a
result does not start an Agent. Explicit activation opens that exact source.

## User workflow

1. Search the sidebar or scope it to a project, running work, or work needing
   attention. Pins precede recent conversations. Provider names, runtime facts
   and recorded history remain separate.
2. Activate a Codex or Claude CLI row. An existing pane for the same source is
   reused. Otherwise the frontend attaches an ordinary terminal pane to the
   shared session host.
3. Use the provider's terminal input directly. There is no second outer
   composer around a CLI TUI.
4. Close the attachment or press **Ctrl+]** to detach. The managed CLI keeps
   running. Activate its row again to reconnect. **Stop** explicitly terminates
   the managed process group.
5. Open an API chat or native task to use its real structured view. The
   composers preserve multiline input: Enter inserts a newline and Ctrl+Enter
   submits. Task creation, execution, validation and acceptance remain distinct.

One attachment owns CLI input at a time. A second application cannot start
another provider for the same managed session. Detach the existing attachment
before attaching elsewhere. Unmanaged CLIs are reported as conflicts when their
exact source can be established; they are not silently killed or adopted.

## Ownership and data model

| Object | Meaning | Owner |
| --- | --- | --- |
| Conversation | Identity, title, pins, tags, draft, API history | Shared SQLite store |
| Provider history | Original transcript and native session ID | Provider CLI |
| Runtime | An executing process and its lifecycle | PTY host or native task runtime |
| Attachment | Terminal client authorized to send input | Host input lease |
| View | Selection, reading position, displayed history, local edits | Frontend |
| Task | Goal, isolated worktree, validation, acceptance | Existing TaskManager |

A source identity includes provider, host, canonical provider home and native
session ID. A UUID alone is insufficient: two accounts can contain the same ID.
Normal and archived copies within one home still identify one conversation.
Catalog discovery preserves equal IDs from different homes. Sidebar activation
and new organization writes use this identity, including older management
dialogs.

The API store uses a different namespace. Shared numeric ChatStore IDs begin at
`1 << 48`; legacy local allocation stays below that boundary. Legacy snapshots
are imported idempotently with durable mappings. An already-shared chat is not
reimported when another window saves it.

The older SessionLibraryStore file remains a compatibility input. New workspace
metadata is authoritative in SQLite. Index refresh updates source facts without
overwriting a shared alias, tags, archive or pin. Organization batches validate
all targets before a transaction begins.

## Modules and deployment

| Module in `agent_workspace` | Responsibility |
| --- | --- |
| `model` | Source identities, sidebar rows, runtime facts, chat records, receipts |
| `client` | Bounded worker queues, requests, embedded host and attachment entry points |
| `protocol` | Versioned Unix-socket messages and frame limits |
| `store` | Single-writer SQLite transactions, migrations, history and draft ownership |
| `ownership` | Private paths, process start identity, inherited kernel leases |
| `host` | Persistent PTYs, terminal snapshots, attachment ownership, cleanup |

Each frontend calls `run_embedded_if_requested()` before GUI initialization.
Its installed executable can launch the host and attachment without a separately
installed helper. The standalone `jterm-sessiond` binary is available for
packaging and integration tests.

`WorkspaceClient::new()` performs no filesystem/socket I/O on the UI thread.
Its worker handles commands with bounded queues: 32 requests and 64 replies.
Frontends correlate replies with the request ID and selected conversation epoch.
Queue admission is not a durability receipt.

State lives at `$XDG_STATE_HOME/jterm/agent-workspace`, falling back to
`~/.local/state/jterm/agent-workspace`. `JTERM_WORKSPACE_STATE_DIR` selects an
explicit absolute root, including isolated test deployments. State/runtime
directories must belong to the current user and have mode 0700. Existing
insecure directories are rejected instead of changing permissions on an
unrelated user-selected path. Long paths use a shorter private endpoint derived
from the state root.

## Shared API conversations

The daemon serializes database writes. SQLite schema version 3 retains verified submission-owner process identity,
per-view reading positions, and durable API execution snapshots; unsupported
future schemas fail explicitly.

A bounded snapshot feeds ChatStore and model context. A separate message table
retains completed exchanges for older-history paging. Draft revisions are
independent of conversation revisions so typing can continue while an answer
streams. Reading positions belong to an identity and view, not a row index.

Submission is an acknowledged transaction:

1. Save the exact snapshot and wait for its revision.
2. Request `BeginSubmission` with conversation revision, draft revision and text.
3. Send `StartApi` with that receipt and the resolved provider configuration.
   The host records admission before releasing its supervised API worker.
4. Observe sequenced previews and the result. The host commits the complete
   user/assistant pair and terminal execution state in one SQLite transaction.
   A lost acknowledgement is resolved by receipt; it never repeats HTTP.

Closing a view drops its observation and releases its control lease. The host
continues the request. Reopening the conversation restores the pending user
turn and current preview without creating another submission. A live view's
control lease cannot be taken by a second view; read-only observation is allowed.
Stop keeps the submission reserved until transport cleanup is acknowledged.
Controller identity includes PID, process start identity and a fresh generation.

The inherited conversation lock survives in worker descendants. After a host
crash the persisted execution is marked Interrupted with an unknown outcome;
it is never automatically replayed. A surviving descendant keeps new execution
locked. Historical execution state cannot activate live controls. Older hosts
advertise no background API capability, so updated frontends retain the draft
and request a host update before beginning a submission.

An old response cannot complete a newer submission, clear a later draft, or
write into another selected chat. The database verifies the real client PID
and start identity through Unix peer credentials. A dead GUI owner can be
recovered while the host remains alive; PID reuse alone grants no ownership.

Older-history pages contain complete user/assistant pairs. Cursors are returned
only when more history exists. Reading older pages does not enlarge model
context. Rename, pin, tag, archive and delete are shared operations. Deleting
conversation metadata does not delete original provider CLI files.

API credentials remain in each frontend's existing API configuration. The
resolved request configuration crosses the private same-user socket and worker
stdin only; it is absent from argv, Debug output and SQLite execution records.
API credentials are not read from CLI login files. An HTTP chat is not
presented as a consumer ChatGPT or Claude web conversation.

## Persistent CLI runtime

`managed_launch()` builds an argv plan for an ordinary terminal pane without
shell interpolation. The daemon revalidates the original cwd, transcript and
provider home before spawning.

The host owns the PTY, controlling terminal and process supervision. Each
attachment has an unguessable epoch; input, resize and detach must match its
current owner. The handshake returns a terminal snapshot and sequence boundary,
followed by sequenced output from that runtime incarnation.

Snapshots preserve primary/alternate screens, incomplete UTF-8/CSI input,
margins, pending wrap, SGR, tab stops, cursor style, application keypad and the
Kitty keyboard stack. Tests reconnect at every byte boundary in a representative
terminal stream. This bounded recovery surface does not replace the provider's
full transcript.

Kernel file locks are inherited by provider processes. Closing a GUI does not
release runtime ownership. If the host dies while a descendant survives, another
host fails closed rather than starting a duplicate. A recorded PID or elapsed
timeout alone is never enough to steal a lease.

`SupervisedChild::spawn_pty_session` establishes the controlling terminal.
Explicit Stop and abnormal cleanup terminate the group, handle TERM-ignoring
descendants, and delay leader reaping until supervision finishes. Failed
startup removes incomplete runtime rows and stops any child already created.
Exited providers are recorded as exited; historical messages are not evidence
of current execution.

## Native providers

### Codex

Native Codex requires the audited **0.147.0** app-server protocol. Preparation
pins the task worktree and executable, validates empty private configuration,
and supplies an in-memory ChatGPT access grant. User trust, refresh tokens,
hooks, plugins, MCP configuration and marketplace state are not copied.

New threads use durable application-owned profiles under `native-profiles`.
A private marker records task, worktree, relative cwd, branch, title and exact
thread ID, with no login material. Provider history survives Finish and restart.

After a clean Finish with verified cleanup, **Resume native** can reopen the
same task in the current application. Preparation checks the original
task/worktree/branch, acquires the profile lease and reads a fresh access grant.
Changed config, linked files and identity changes are refused. `thread/resume`
rebinds persisted cwd/runtime roots to fresh pinned descriptors. It does not
resubmit the original goal.

History uses `thread/turns/list` pages of eight. Bounded historical
user/assistant text is separate from live events. Old tools, approvals and file
changes never become current actions. The view keeps at most 64 turns / 1 MiB;
the provider retains full history. **Load earlier** follows its cursor and
refresh starts at the latest page. A history-read error is shown without
making the loaded thread unusable.

Resume installs a new local stream epoch. Old worker events cannot affect it.
Prior validation is invalidated and a new turn requires an explicit send.
Existing containment and approval gates remain enforced.

Native profiles also appear in the CLI sidebar. The marker supplies durable
cwd instead of the obsolete `/proc/self/fd/N` from provider history. Terminal
launch uses the same `CODEX_HOME` and explicit `--cd`. The profile lease
excludes simultaneous native and CLI runtimes. An isolated profile may need
its own CLI login; native access grants are not persisted as CLI refresh
credentials.

Protocol source:
[Codex rust-v0.147.0 thread protocol](https://github.com/openai/codex/blob/rust-v0.147.0/codex-rs/app-server-protocol/src/protocol/v2/thread.rs).

### Claude

The native conversation driver supports sequential turns. First use reserves
one UUID with `--session-id`. Later turns use `--resume` with that UUID and
the original provider home. Each turn owns a real child process; its predecessor
must be stopped and reaped first. The session lease stays held while idle.

One SessionStarted covers the conversation; each turn has its own start and
completion. SessionEnded occurs only on Finish, cancellation or failure.
Duplicate sends are atomically rejected, and a mismatched provider ID fails.

The print protocol has no interactive approval channel. It uses
`--permission-mode dontAsk` and reports permission denials with an actionable
terminal-continuation message. It does not enable
`--dangerously-skip-permissions`. Text and retained turns use the existing
bounded structured view.

Provider flags follow the official
[Claude Code CLI reference](https://code.claude.com/docs/en/cli-reference).
Kimi remains one-shot; different capabilities are not hidden behind a universal
resume button.

## Lifecycle boundaries

The persistent host owns managed CLI PTYs and supervised API HTTP requests.
Updated API chat views detach on close and can reattach from another frontend.
Structured native Task runtimes still remain frontend-owned in this revision;
closing their frontend stops them. Task instructions and validation state are
not silently serialized into the shared index.

The existing native failure recovery action **Continue in terminal** launches a
new CLI task prompt after verified shutdown. It is not the same operation as
selecting an indexed thread. Exact transcript continuation uses that
source-qualified row or Codex's verified native resume. The UI must not present
new-goal compatibility recovery as same-thread migration.

Consumer ChatGPT/Claude desktop/web histories are not automatically imported or
synchronized. Providers retain their own authentication, transcript formats and
session IDs. Changing provider creates a new conversation identity.

## Verification

Coverage includes real host/PTY startup, killed attachment recovery to the same
PID, duplicate writer rejection, detach and descendant cleanup; two API clients
with exact receipts, stale owners and newer drafts; real localhost HTTP workers
surviving frontend exit, controller handover, Drop-detach, cancellation cleanup,
unknown outcomes after host crash, receipt resolution and credential nonpersistence;
idempotent migration,
source identities, complete-pair history, atomic metadata, private paths; screen
reconstruction across partial input; actual fake-Claude processes across turns;
and Codex resume/provenance/history/stale-stream protocol checks.

Process, `/proc` and Unix-socket tests need an environment permitting those
operations. Sandbox denial is not evidence of product success or failure. The
pre-existing cgroup test requires a functioning user cgroup manager. The
unchanged baseline also has a Git metadata probe failure in the managed build
container; these are recorded separately from feature regressions.

Real provider authentication and network requests need a smoke test with the
user's supported CLI/login. Fake-provider tests verify process/protocol behavior
without paid requests or claims of live account access.
