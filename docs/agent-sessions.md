# CLI agent sessions

`jterm_core::agent_sessions` supplies a native-toolkit-independent, read-only
index of existing Codex and Claude CLI conversations. It is separate from
`agent_task`: creating an experimental native task is not required to find or
resume an ordinary CLI conversation.

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
`query_with_library` adds pinned-only and previously-resumed filters from a
shared `SessionLibrarySnapshot`. `RecentlyResumed` sorts by the receipt
timestamp, retaining deterministic base order when timestamps tie. Optional
pin-first ordering is honored only for empty-query `Relevance` browsing;
search matches and explicit chronological/live sorts retain their meaning.

## Shared pins and resume receipts

`SessionLibraryStore::new()` starts a background load of
`$XDG_STATE_HOME/jterm/agent-sessions-library.json` (falling back to
`~/.local/state/jterm/agent-sessions-library.json`). All four terminals use the
same path. The file stores only provider, UUID, pin state and the most recent
resume timestamp, never prompts, previews, project paths or credentials.

The constructor, `set_pinned`, `record_resumed`, `refresh`, `poll`, and snapshot
queries perform no filesystem I/O on the caller's thread. One worker drains a
bounded queue of up to 256 distinct pending identities; updates for the same
identity coalesce. Workers continue saving without UI polling, including
queued work when the store is dropped. A process killed before its background
write finishes can still lose a pending update; hosts should show Saving while
`is_busy()` is true rather than promise an immediate durable write.

Each transaction acquires a cross-process directory lock with a two-second
wait limit, reads current metadata, merges only that transaction's operations,
and atomically replaces the private file. One terminal never writes its old
whole snapshot over another terminal's pins. Delayed resume receipts cannot
move a saved timestamp backwards. Directory and file modes are 0700 and 0600;
symlink, unsafe permission, malformed, duplicate-key and future-version files
are rejected and left unchanged. The version-1 JSON is capped at 1 MiB and
4,096 identities. Errors are nonfatal and exposed by `error()` after `poll()`.

Poll to adopt completed snapshots; refresh periodically while open to observe
other terminal windows' changes. Re-query cached search indices when the
snapshot changes. Record a resume only after the host has successfully created
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
a different directory. Archived sessions are visible but require the user to
unarchive them in Codex before resuming.

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
older files after a partial first scan. Codex title indices have a separate
2 MiB-per-store tail ceiling. Warnings disclose sampling or budget
omissions. The cache is not a persistent index, and a cold scan can omit older
history until refreshed. No provider transcript, home, or archive is modified.
