# CLI agent sessions

`jterm_core::agent_sessions` supplies a native-toolkit-independent, read-only
index of existing Codex and Claude CLI conversations. It is separate from
`agent_task`: creating an experimental native task is not required to find or
resume an ordinary CLI conversation.

## Agent management workspace

The four native managers share personal organization: give a conversation a
recognizable name, add comma-separated tags, pin it, or archive it locally.
Use **Active library**, **Archived here**, or **All conversations** to change the
view, and select a project directory to include its descendants. Project summaries
show active conversations, recorded input requests, observed live CLIs, pins and
local archives. Recorded input requests and live processes remain separate facts.

Names and tags are saved explicitly. UI adapters retain an identity-bound draft
while the catalog refreshes, and show saving/error feedback. An empty name restores
the provider title; an empty tag field removes the tags. Names allow 160 UTF-8
bytes, and up to 12 tags of 32 bytes each are normalized, deduplicated and sorted.
Control and invisible formatting characters are rejected before saving.

Local archive is reversible organization, independent of the provider's own
archive and of native task completion. It hides a conversation from the default
view without deleting its history or affecting an active process. Restoring a
local archive does not unarchive a provider-owned Codex transcript.

For isolated native tasks, `agent_task::DashboardSnapshot` adds five presentation
orders: needs-you first, recent update, oldest update, title and project.
`next_attention` cycles the currently visible tasks needing input or review;
`step_selection` navigates visible stable identities. These functions only
return selections. TaskManager and approval paths still decide what operations
can execute. A filter that hides the selection never selects another task
automatically.

The [task command center](agent-task-command-center.md) adds separate approval,
reply, validation, review and failure queues, shared structured task queries,
and runtime-aware next-step explanations. Task state, validation results and
observed runtime activity stay separate; a passing check does not complete a
task. The same guide describes the four native interfaces and follow-up draft
acknowledgements.

### Bulk organization

The managers keep the inspected conversation separate from the checked batch.
Select visible conversations, review the selected/hidden counts, and explicitly
pin, unpin, archive, restore, add one tag, or remove one tag. A selection is bound
to provider and UUID, not to row positions or a later search result. Selection,
search and tag editing do not resume conversations.

`SessionLibraryStore::apply_batch` validates all identities and admits at most
`SESSION_BATCH_LIMIT` (256) targets together. An invalid identity or insufficient
queue capacity admits none. The worker merges relative tag changes against the
latest private file under the process lock, preserving aliases, history, pins
and unrelated tags. If any target exceeds a metadata limit, the entire disk
transaction fails without a partial write. UI adapters retain their selection
and show the error until the user can retry. A successful enqueue is not a
durability receipt; polling completion remains required.

### Project task creation and review

`TaskBrief` represents a user's explicit goal, independently of captured
`SemanticCommandContext`. The four task panels can create an isolated worktree
from a title, project, provider and goal without requiring a failed command.
`TaskManager::create_with_brief` registers it in `Created` with no runtime or
terminal binding. Starting the provider is a separate action for every provider.

Goals allow 16 KiB of UTF-8 with structural newline/tab characters. Native and
PTY task prompts validate the goal, honor existing sharing/redaction policy and
identify its authored provenance. Native preparation pins the managed worktree
and uses its repository root; it does not invent shell evidence. An optional
`TaskValidationRecipe` captures one exact command and the absolute configured
shell. Validation uses the same containment and pinned-directory checks as
command-created tasks and requires a separate explicit action.

A task with a recipe retains the existing requirement that its latest validation
pass before acceptance. A task authored without a recipe can instead be accepted
through `complete_after_manual_review`, only after it is ready for review, its
native stream has ended and no validation has run. This explicit human-review
path cannot complete legacy command tasks or tasks with a configured recipe,
and it never records a fictitious passing validation.

`TaskInspector` supplies bounded project/worktree, branch, immutable baseline,
origin, runtime, actual validation/exit facts, timestamps and next-step guidance.
It is a presentation of current evidence, not a synthetic lifecycle history or
an action capability. Frontends may additionally display actual retained native
turn/file history and its truncation indicators.

Authored goals are retained in the running task. Existing frontend recovery
journals intentionally omit task instructions and terminal output; this upgrade
does not silently widen that persistence contract. To continue authored work
after restart, re-enter the goal in a new task; recovery can still expose the
retained worktree metadata. The session library
continues to store organization only, never authored task goals.

## Discovery and presentation

`SessionScanner::refresh` starts one background scan. Hosts poll it from their
event loop, keep the existing catalog while a scan runs, and replace it only
when `poll` returns a catalog. Selection uses `(SessionProvider, id)`, not row
position. Hosts should also use this identity to focus an already resumed,
still-live native terminal tab instead of opening another copy.

Discovery combines the default `~/.codex` and `~/.claude` homes with configured
`CODEX_HOME` and `CLAUDE_CONFIG_DIR` homes. It reads Codex `sessions` and
`archived_sessions`, and Claude `projects/*/*.jsonl`. Nested Claude subagent
logs and sidechain records do not become independent user conversations.
Compressed Codex `.jsonl.zst` files are supported.

Each row carries the provider, stable UUID, bounded title and recent text
preview, original project directory, branch when available, modification time,
source file, and last recorded transcript state. Codex custom names from each
store’s bounded `session_index.jsonl` override inferred first-prompt titles;
newer physical entries win for the exact UUID and source store.
`matches_query` requires all
whitespace-separated search terms to match this metadata. This is metadata
and preview search, not an exhaustive full-transcript search.

Status deliberately distinguishes historical evidence from process liveness:

- `Working`: the **last recorded** event started work or used a tool.
- `Waiting`: a recorded explicit input/approval request.
- `Idle`: a recorded completed turn; ready to resume.
- `Unknown`: no reliable final event or only a compressed prefix was sampled.
- `Archived`: the source belongs to Codex's archived store.

On Linux, each scan also inspects bounded current-user `/proc` command lines.
An exact CLI executable or its Node launcher plus an explicit UUID in
`codex resume UUID`, `claude --resume UUID`, `-r UUID`, or `--session-id UUID`
produces a `LiveSessionProcess`. A fresh CLI without a UUID can also be
correlated through an open file descriptor exactly matching an indexed history
source and provider. This checks at most 256 descriptors per process and 4,096
per scan; working-directory similarity never creates a match.
`catalog.live_pids(session)` exposes this separate evidence. An alive PID does not prove an agent is generating tokens,
waiting for a permission, or idle. A CLI that holds no matching history descriptor
may therefore have no correlated PID even when a process is alive.

## Ranked query projection

All four native adapters can share `SessionSearchIndex` instead of rebuilding
search text or duplicating filtering rules for each rendered row. Construct the
index once from a changed `SessionCatalog`, then retain its returned catalog
indices for the active query. `AgentSession` and `SessionCatalog` implement
`PartialEq`/`Eq`, so unchanged discovery snapshots can be ignored without
resetting selection, scroll position or row widgets.

`SessionQuery` combines text, provider, recorded status, live-only filtering,
an optional absolute project directory, and `SessionSort`. Every whitespace-
separated term must match an indexed field. Relevance gives exact session IDs
first priority, followed by ID/title matches, project/branch matches, then
recent-message previews. It is not a full-transcript or fuzzy semantic search.
Search text uses Unicode lowercase matching; stable provider/ID keys break
remaining timestamp ties independently of catalog row order.

`Relevance` is the default; an empty query retains recent-first ordering.
`Recent` always orders matches by their modification timestamp. `LiveFirst`
puts positively observed processes first, then relevance and recency. The
recorded activity value remains distinct from process liveness in every mode.

A project scope includes its lexical descendants: `/work/repo` includes
`/work/repo/subdir`, excludes `/work/repo-other`, and normalizes `.`/`..`
components. Relative scopes return no matches. Resolving symlink aliases or
finding repository roots belongs to the host; query evaluation never reads the
filesystem or starts Git. Queries over 4,096 bytes or 64 terms return no matches
rather than ignoring required terms.

The convenience `catalog.query(&query)` builds a temporary index for one-shot
callers. GUI event loops should cache `SessionSearchIndex::new(&catalog)` and
re-query only after text/filter/sort changes or an actual catalog replacement.
`query_with_library` adds local-archive, pinned-only and previously-resumed filters from a
shared `SessionLibrarySnapshot`. `RecentlyResumed` sorts by the receipt
timestamp, retaining deterministic base order when timestamps tie. Optional
pin-first ordering is honored only for empty-query `Relevance` browsing;
search matches and explicit chronological/live sorts retain their meaning.

The same query parser supports aliases, tags, phrases and field constraints:

| Search | Matches |
| --- | --- |
| `renderer gui` | Both terms across title, alias, tags, project, branch or preview |
| `provider:codex tag:review` | Codex conversations with the exact normalized tag |
| `project:/work/repo` | That absolute directory and its lexical descendants |
| `title:"renderer redesign"` | A phrase in the provider title or local alias |
| `branch:feature -tag:done` | Matching branch, excluding the exact tag |
| `is:live is:pinned` | An observed live CLI that is also pinned |
| `is:archived` | Locally archived conversations, including from the initial Active view |
| `status:waiting` | Last recorded input request, independently of liveness |
| `status:archived` | Provider-archived transcripts, independently of local archive |

`name:` aliases `title:`, `cwd:` aliases `project:`, and `id:` and `is:resumed`
are also supported. All constraints intersect. A minus outside quotes excludes
a term. Within double quotes, `\"` and `\\` escape a quote or backslash. Unknown
field prefixes remain literal, preserving URLs and ordinary colon-containing
text. Invalid or over-limit queries return no matches; `query_error` supplies
the native UI explanation. These are metadata queries, never shell commands.

## Shared pins and resume receipts

`SessionLibraryStore::new()` starts a background load of
`$XDG_STATE_HOME/jterm/agent-sessions-library.json` (falling back to
`~/.local/state/jterm/agent-sessions-library.json`). All four terminals use the
same path. The file stores provider, UUID, pin state, the most recent resume
timestamp, and optional user-authored alias/tags/local archive state. Provider
prompts, previews, project paths and credentials are never copied into it.

The constructor, `set_pinned`, `set_details`, `set_archived`, `record_resumed`,
`refresh`, `poll`, and snapshot
queries perform no filesystem I/O on the caller's thread. One worker drains a
bounded queue of up to 256 distinct pending identities; updates for the same
identity coalesce. Workers continue saving without UI polling, including
queued work when the store is dropped. A process killed before its background
write finishes can still lose a pending update; hosts should show Saving while
`is_saving()` is true rather than promise an immediate durable write.

`is_saving()` covers only user changes: queued, being written, or finished but
not yet acknowledged by `poll()`. The periodic read-only reload that observes
other windows never counts, so hosts gate Save, pin, archive and batch controls
on `is_saving()` and the controls no longer disable themselves on every
refresh. `is_busy()` still includes that reload and any unacknowledged result;
use it only to decide how often to poll. A failed write stays in `error()` after
`poll()` even when a later reload finished first, so a refresh can never turn a
failed save into an apparent success.

`SessionLibrarySnapshot::facts(provider, id)` returns pin, resume receipt,
local archive, alias and tags from one identity lookup; row renderers should use
it instead of separate getters, each of which rebuilds the identity key.
`validate_session_details(alias, tags)` and `validate_session_batch_tag(tag)`
apply exactly the checks that `set_details` and `apply_batch` will, without
queueing anything, so editors can explain a byte limit while the user types.
Limits are UTF-8 bytes (`SESSION_ALIAS_MAX_BYTES`, `SESSION_TAG_MAX_BYTES`,
`SESSION_TAG_LIMIT`), not characters.

Each transaction acquires a cross-process directory lock with a two-second
wait limit, reads current metadata, merges only that transaction's operations,
and atomically replaces the private file. One terminal never writes its old
whole snapshot over another terminal's organization. Alias and tags save together;
pin, archive and resume receipt updates merge independently. Concurrent edits to
the same alias/tag pair use the last completed transaction. Delayed resume receipts cannot
move a saved timestamp backwards. Directory and file modes are 0700 and 0600;
symlink, unsafe permission, malformed, duplicate-key and future-version files
are rejected and left unchanged. Version-1 metadata is accepted and migrated to
version 2 on a successful write, preserving pins and resume receipts. Older
version-1-only binaries reject the new format instead of overwriting it; update
all four frontends together. JSON remains capped at 1 MiB and 4,096 identities.
Errors are nonfatal and exposed by `error()` after `poll()`.

Poll to adopt completed snapshots; refresh periodically while open to observe
other terminal windows' changes. `SessionScanner::refresh_if_idle_for(interval)`
starts a background scan only when none is running and the previous one
finished at least `interval` ago, so a history slower than the interval never
causes back-to-back rescans; reload the library only when it returns `true`.
Explicit refreshes (opening the manager, F5, a Refresh button) should rescan,
reload the library and recheck readiness together. Re-query cached search
indices when the snapshot changes. Record a resume only after the host has successfully created
the PTY or focused a valid existing session tab, not when selecting a row or
starting a readiness check. This is a launch/switch receipt, not proof that a
model turn completed successfully.

## Resuming

After the user selects and confirms a session, obtain a fresh, selection-bound
preflight result and launch its `program` plus `args` as argv in a new native
PTY at `cwd`. Never replace this with `--last`: the chosen UUID must survive
selection and refresh.

The launch plan validates the session UUID, source file, and original absolute
project directory. If the source belongs to a different provider home than the
inherited environment, the plan uses argv such as
`env CODEX_HOME=/original/home codex resume UUID`. This keeps the provider's
original session store and credentials configuration in effect without reading
credentials. The Claude equivalent uses `CLAUDE_CONFIG_DIR`.

`display_command` shell-quotes each argument for a review/confirmation view.
Native pickers use `SessionPreflight` to run `resume_launch` and
`resolve_for_spawn` on a worker, bound to the selected provider/UUID, source,
cwd and archive state. A changed selection or confirmation refresh invalidates
old results. At most one worker and one replaceable latest request exist;
readiness never executes the CLI or a shell profile. Hosts require a fresh,
matching `Ready` result before creating a new PTY, and display `Unavailable`
without opening an empty tab.

`launch.resolve_for_spawn()` performs the activation-only executable lookup.
It finds absolute Codex/Claude executables in the inherited absolute PATH entries, common user
bin directories, and up to 128 NVM version directories. NVM's bounded default
alias is preferred, followed by descending numeric versions. Relative/empty
PATH entries and shell-profile evaluation are excluded.

The resolved argv uses an absolute `env` helper and explicit child PATH. It
keeps the chosen CLI's bin directory and matching Node runtime available for
`#!/usr/bin/env node` npm launchers, including a local-bin symlink into an NVM
installation. Original provider-home assignments, cwd and session UUID remain
unchanged. Discovery and command previews do not resolve executables.

Hosts still launch argv directly. Missing project directories and deleted
history produce a recoverable error; the manager does not silently resume in
a different directory.

`SessionCatalog::resume_conflict(session)` explains when the latest scan
positively observed a CLI process already holding that conversation. Resuming
it again would start a second CLI writing to the same history. A host that owns
the matching tab switches to it; otherwise the first Resume only shows the
warning, and a separate, explicit "Resume anyway" bound to the same provider and
UUID proceeds. Changing the selection, a refresh that removes the conversation,
or Escape withdraws the warning. A repeat within `SESSION_RESUME_ANYWAY_DELAY`
(600 ms) of showing the warning is ignored, so a double-click or a quick double
Enter cannot confirm it unread. A confirmation covers only the attempt it
started: when that readiness check fails or the attempt is abandoned, the next
Resume warns again. The evidence can be one scan old, so it is a warning rather
than a lock.

A background refresh must never move the selection to a different conversation:
when the inspected conversation leaves the view (another window archived it,
its recorded state no longer matches a filter), hosts keep its identity, show
that it is no longer listed, and disable Resume and organization until the user
selects again. Explicit filter or search edits may still select the first match.
Display and copy the exact command from `SessionPreflight::command(session)`,
which includes any `env CODEX_HOME=…` or `env CLAUDE_CONFIG_DIR=…` prefix. The
background check computes it together with readiness, so hosts never call
`resume_launch()` (which checks the project directory and history file) on the
UI thread. It is `None` until the selected conversation's first check finishes
and when the conversation itself cannot be resumed; a refresh keeps the previous
command until the new result arrives. Archived sessions are visible but require
the user to unarchive them in Codex before resuming.

## Bounds and limitations

A scan visits at most 32,768 directory entries and selects at most 2,048 newest
candidate files, using a 128 MiB read/decompression budget. Plain JSONL reads a
64 KiB prefix and tail; a large first metadata record can expand the prefix to
1 MiB. The zstd decoder bounds compressed input to 8 MiB and its window to
8 MiB. Small compressed files may expand up to 2 MiB; large compressed files
normally use a 64 KiB prefix, extending for a large first metadata record.
A compressed prefix never supplies an authoritative last state.

A process-local cache retains at most 4,096 bounded metadata entries keyed by
source path, nanosecond modification time, and file length. Unchanged files do
not consume the next scan's transcript budget, so refresh can index additional
older files after a partial first scan. After each scan, entries for files under
that scan's history roots that it no longer selected (deleted, renamed or past
the newest-file ceiling) are dropped; other roots' entries are untouched. A
long-running terminal therefore no longer reaches the ceiling and discards the
whole cache, which used to force a full re-read. Codex title indices have a separate
2 MiB-per-store tail ceiling. Warnings disclose sampling or budget
omissions. The cache is not a persistent index, and a cold scan can omit older
history until refreshed. No provider transcript, home, or archive is modified.

## Management upgrade validation (2026-10-25)

Session search now resolves every library fact for a query with one sorted
merge between the cached index and the library's ordered maps, instead of
allocating an identity key for each pin, archive, alias, tag and receipt lookup;
project summaries use ancestor slots computed when the catalog changes. On a
release build with 2,048 conversations and 1,024 library identities, empty
browsing with pins first went from 4.2 ms to 0.2 ms per query, recently-resumed
ordering from 5.7 ms to 0.2 ms, `tag:` and `is:` filters from about 1.1–1.5 ms
to 0.1 ms, and project summaries from 5.6 ms to 0.1 ms. A differential run
against the previous implementation compared 240,000 random queries over 20,000
random catalogs and libraries (mixed-case UUIDs, aliases, tags, local and
provider archives, live processes, project scopes, every sort and filter, and
invalid queries): indices and summaries were identical.

The core suites added coverage for `is_saving()` during reloads, a reload
finishing before a failed write is polled, editor and batch-tag validation
matching store admission, one-lookup row facts, completion-based scan cadence,
scoped cache pruning and resume-conflict text. The four native managers adopt
these APIs; their own tests cover the second resume confirmation, selections
that a refresh hides and later shows again, and exact copied commands.
