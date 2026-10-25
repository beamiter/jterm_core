# Agent task command center

The task dashboards in Anvil, Forge, Ember and Frost use one shared query,
attention classification and next-step explanation. These projections help
find and inspect work. The task manager and live runtime still decide whether
an operation can execute.

## Find the work that needs you

The attention queues split the existing **Needs you** view into five reasons:

| Queue | Current evidence |
| --- | --- |
| Approvals | The task is waiting for an approval response |
| Needs reply | The task is waiting for human input |
| Validation issues | The most recent validation failed, was inconclusive, or was cancelled |
| Ready to review | The task is ready for review, with no higher-priority attention reason |
| Failed tasks | The Agent task failed, even if an older validation also failed |

Each task appears in at most one queue. A current approval, question or failed
Agent run takes precedence over an old validation issue. A running validation does not request
acceptance. Archived tasks do not enter the queues. A **Ready to review** task
may still have an open native session; its next-step card explains that the
session must be finished before validation or acceptance.

Queue selection preserves the typed query, provider and project scope. Counts
describe that search scope before selecting a queue or quick view. Selecting
an ordinary quick view clears the queue selection. Clear filters removes the
queue selection as well. Filtering never silently replaces a selected TaskId
with the task that happens to occupy the same row position.

The GTK frontends use a counted attention dropdown to fit narrow panels;
Ember and Frost expose queue buttons. These are alternative presentations of
the same classification.

All four dashboards offer **Previous needs you** and **Next needs you**.
They move in the currently displayed order, wrap at the list edge and skip
tasks that do not need attention. They respect every active filter. A hidden
selection starts from the last matching task when moving backward, or the first
when moving forward; navigation never starts, approves or accepts a task.
Frost keeps each task's follow-up draft while moving in either direction.

The additional **Needs you · oldest update** sort puts attention tasks first
and orders older updates before newer ones within each activity bucket. This
helps revisit older requests without losing the distinction between attention,
active work and finished work. The time is the task's recorded last update,
not an inferred approval waiting duration. The existing **Needs you first**
default continues to put the newest updates first. Changing either sort keeps
the selected task identity and filter counts unchanged. GTK panels place sort
and navigation on separate rows so the extra controls fit narrow sidebars.

**Selected project** narrows any of the four dashboards to the selected
task's original repository path. It keeps the current query, provider, quick
view, attention reason, sort and selected TaskId. The project selector reflects
the resulting scope. A selection hidden by filters, a removed task or missing
project metadata leaves the button unavailable; activation checks again.
Display labels and lossy path text never identify a project, including when
two non-UTF-8 paths look identical. The button changes view state only.

## Task search

Words and constraints combine with AND. Double quotes preserve a phrase, and
a leading minus outside quotes excludes a term. Enumerated fields accept
comma-separated alternatives with OR. Text fields preserve commas literally.

| Query | Result |
| --- | --- |
| `provider:codex status:review` | Codex tasks whose lifecycle is ready for review |
| `provider:codex,claude -status:completed` | Either provider, excluding completed tasks |
| `attention:approval` | Tasks currently requiring an approval response |
| `attention:validation validation:failed,inconclusive` | Failed or inconclusive validation needing attention |
| `project:"team repo" branch:agent` | A displayed project phrase and branch substring |
| `runtime:native is:running` | Native tasks with running lifecycle, validation or positively observed runtime activity |
| `status:review is:agent-stream` | Review tasks retaining an open native stream |
| `is:validation-running` | Tasks with a running validation attempt |
| `title:"input draft" -provider:kimi` | A title phrase, excluding Kimi |
| `id:7fd3` | A task UUID containing the supplied fragment |
| `"status:failed"` | Literal text containing a colon, rather than a state filter |

Supported fields and canonical values:

| Field | Values |
| --- | --- |
| `provider:` | `codex`, `claude`, `opencode`, `kimi` |
| `status:` | `created`, `starting`, `working`, `approval`, `input`, `review`, `completed`, `failed`, `cancelled`, `archived` |
| `runtime:` | `unassigned`, `terminal`, `native`, `fallback` |
| `validation:` | `not-run`, `running`, `passed`, `failed`, `inconclusive`, `cancelled` |
| `attention:` | `approval`, `input`, `validation`, `review`, `failed` |
| `is:` | `attention`, `running`, `not-started`, `review`, `finished`, `archived`, `preparing`, `agent-stream`, `validation-running` |
| `project:`, `branch:`, `title:`, `id:` | Case-insensitive displayed-text substring |

`state:` aliases `status:`; `repo:` and `workspace:` alias `project:`.
Long status names such as `waiting_for_approval` and `ready-for-review` also
work. `status:working` is a lifecycle constraint; `is:running` additionally
includes validation and retained runtime activity. These facts must not be
treated as interchangeable.

Task `project:` search is a displayed-text match. The project picker separately
matches the original exact path and remains the way to select one repository
identity. A query cannot widen that scope. Session-library project search has
its own directory-and-descendant semantics; it is documented separately in
[CLI agent sessions](agent-sessions.md).

URLs and Windows paths remain literal search text. Quote other literal text
containing colons to avoid interpreting it as a filter. Invalid fields, unknown
enumerated values, unclosed quotes, hidden control characters and excessive
query size produce an inline error and zero results. Constraints are never
silently dropped. Queries are limited to 256 UTF-8 bytes and 32 terms.

Search reads bounded task metadata already held in memory. It does not invoke
the shell, scan worktrees, inspect secrets or search full Agent transcripts.

## Understand the next step

`TaskGuidance::from_task(task, activity)` supplies a shared title, explanation
and bounded current error or question. Live activity comes from each frontend's
actual terminal/runtime owners. A saved terminal ID alone does not prove that
the terminal is still available.

The guidance distinguishes:

- A created task from a started Agent, including a recovered worktree that has
  no retained task instructions.
- A pending approval or question from background preparation.
- A reviewable result with a live native session from a finished session that
  can be validated.
- A running, passed, failed, inconclusive or cancelled validation from task
  acceptance.
- Explicit manual review for authored tasks without a validation recipe from
  the validation requirement for tasks that have one.
- Failed or cancelled runs from accepted work. Existing worktree changes remain
  available for inspection.

Frontends show only operations allowed by their current runtime gates. A queue
label, guidance message, passing validation or button highlight never grants
approval or automatically accepts work. Unsupported reply and approval paths
remain unavailable.

## Native interface changes

Anvil and Forge put a contextual next-step card and a primary available action
above the full action controls. Follow-up text stays in its task-bound draft
until the runtime accepts the exact submission. Each attempt has a distinct
receipt identity: failure retains the draft, a late receipt cannot acknowledge
a retry, and success does not erase newer text or another task's input.

Ember task cards display lifecycle, validation, runtime activity and update age
as separate facts. A validation pass no longer displays the completed-task
checkmark. The completion marker is reserved for explicitly completed tasks.

Frost separates the list and inspector so long task lists do not bury selected
task operations. Wide panels show independent scrolling panes; narrow panels
switch between the list and the selected task. Actions and current requests
precede retained history. Hidden inspectors cannot dispatch stale task actions.

This change retains the existing persistence contract. Task instructions and
runtime transcripts are not newly written into recovery journals. See the
project-task recovery limitations in [CLI agent sessions](agent-sessions.md).

## Hidden tasks and restore

Hiding a task (`TaskManager::archive`) is reversible. The manager records the
lifecycle state in `AgentTask::archived_from`, and `TaskManager::restore`
returns the task exactly there. Restoring never starts an Agent, reruns
validation or accepts work; validation records, exit facts, terminal bindings
and the worktree are unchanged. Hiding an already hidden task is a no-op. A
record hidden by an older build has no preserved state and reports
`TaskError::ArchivedStateUnknown` instead of guessing; create a new task for
its worktree. The field is optional in serialized tasks, so older records load
unchanged.

`TaskManager::can_archive` answers whether Hide would succeed now without
changing anything. Dashboards disable Hide while an Agent stream, a running
lifecycle or a validation is active, and say what has to finish first.
`archived_count` sizes a "Hidden tasks (N)" view. All four dashboards keep
hidden tasks out of every ordinary view, queue and badge; the hidden view lists
only them, offers Restore and read-only inspection, and returns to the ordinary
view once nothing hidden remains.

## Attention outside the panel

`TaskManager::attention_counts()` classifies visible tasks into the same five
disjoint queues as dashboard rows, without projecting a dashboard.
`DashboardAttentionCounts::total()` gives the badge number and `summary()` a
bounded explanation such as `2 tasks need you: 1 approval, 1 reply`. Anvil and
Forge show a Tasks button with this count in the top bar, Ember a Tasks chip and
the sidebar label, Frost the Tasks switcher and Agents button. The badge is a
pointer to work, not an approval or acceptance path. Ember and Frost add the
`task:toggle` command on Ctrl+Alt+Shift+T, matching the existing Anvil and Forge
shortcut.

## Native views and polling

Provider workers now publish a process-unique revision whenever they change the
live view. `AgentRuntimeManager::snapshot_revision(task)` reads it without
copying anything, and `shared_snapshot(task)` returns the same `Arc` until the
view changes. Its content is identical to `snapshot(task)`. Frame- or
tick-driven dashboards use the shared form, so an idle task no longer copies
its assistant text, command output and file diffs several times per frame.
Revisions never repeat for a later run, and a retained final view keeps the
revision of the last live view.

`needs_fast_poll()` is false while a native session only waits for an approval
(as it already was at an idle review point). Nothing arrives until the user
responds, so the low-frequency lifecycle tick is enough; hosts poll promptly for
a short period after the user's own approve, deny, follow-up, finish or stop
action instead of polling at frame rate for as long as an approval is pending.
`CodexAppServerPhase`, `CodexAppServerApprovalKind` and
`CodexAppServerExitCause` have readable `label()`s for native task views.

Task search normalizes displayed row text only when a term actually compares
text; enumerated constraints such as `status:` or `provider:` no longer copy
nine lowercase strings per row.

## Projection costs

The dashboard separates searching a scope from narrowing its visible rows.
`project_unsorted` and `filter_iter_unsorted` build the searched scope without
an intermediate sort; `view` or `apply_view` chooses the quick view, attention
reason and final order while keeping scope counts intact. Existing `project`
and `filter_rows` retain their earlier default ordering for other callers.
`attention_counts` classifies each source row once for all five queue badges.

Anvil and Forge reuse one searched scope for both badges and task rows,
cloning only matching candidates. Ember and Frost no longer parse an empty
query or sort by the default order before applying the selected order. Frost
also resolves its domain scope once per dashboard frame, with one inventory
of live terminal identities rather than rescanning sessions for every task
and terminal role. This inventory is rebuilt for each projection; focus and
execution still perform their existing live ownership checks.

Numeric sort modes sort in place and compare UUID values directly, avoiding
a formatted UUID allocation per row. UUID numeric order equals the previous
canonical text order; title/project modes still cache their case-folded text
keys. No task state, query bounds, repository identity or action gate changes.

## Core validation (2026-10-24)

The Agent domain regression suite passed **254 tests** with one host-dependent
test explicitly excluded, using `cargo test --offline --locked --lib
agent_task:: -- --test-threads=1 --skip
agent_task::drivers::codex_app_server::tests::cgroup_scope_kills_term_ignoring_setsid_descendant_before_reap`.
New coverage exercises structured and invalid queries, literal/quoted/Unicode
text, negative constraints, exact workspace intersections, all lifecycle and
validation combinations for attention classification, live-session guidance,
and bounded failure explanations. Strict all-target Clippy (`-D warnings`) and
format checks passed.

The excluded systemd/cgroup integration test failed in this container with
`cannot inspect app-server cgroup`. The identical test also failed on the
unmodified `66a2555bb1a58cbd8230f2dea875d06c5544b9c0` baseline. A parallel run
additionally exposed an intermittent existing Claude process-cleanup test;
its isolated rerun and the full serial regression passed. No process-management
implementation or test was changed to obtain these results.

## Navigation upgrade validation (2026-10-25)

The Agent domain suite passed **256 tests** serially with the same existing
cgroup test excluded. New coverage checks oldest-update attention ordering,
bidirectional wraparound, hidden selections, empty queues, stable identities
and unchanged lifecycle/counts. The suite ran outside the filesystem sandbox:
the sandbox's mapped parent-directory ownership otherwise causes eight
unchanged launcher trust checks to reject their temporary fixtures.

Frost's Agent panel suite passed **28 tests**, including preserved follow-up
drafts during backward attention navigation. Ember passed **12** management
and dashboard tests. Anvil and Forge each passed their display-backed dashboard
regression with both attention controls and hidden-task rejection exercised.
Anvil, Forge and Ember all-target checks, core strict Clippy, formatting and whitespace checks
passed. GTK checks used the existing offline Nix development environment;
the system GTK/GLib versions are older than the projects require. Nix release
builds for Anvil and Forge completed with private family-runner lockfiles;
their version probes passed and their dynamic libraries all resolved.

## Scope and project-focus validation (2026-10-25)

The follow-up Agent domain suite passed **258 tests**, with the same existing
cgroup test excluded. The new scope tests compare derived views against full
projections across all six orders, all six quick views and every attention
reason, including invalid queries and archived validation failures. They also
check that switching views preserves scope counts and task identities without
resolving live activity again. Strict all-target core Clippy passed.

Anvil and Forge each passed **seven** task-filter and display-backed dashboard
checks, including lossless non-UTF-8 project identities, 1,000-task ordering,
project focus preserving query/provider/selection and hidden-selection refusal.
The new display regression is part of each frontend's explicit GTK test script.
Frost's **28** Agent panel tests passed with live-inventory replacement and
finished-validation checks; Ember's **37** task UI tests and all-target check
passed. The scope optimization keeps the former ordering and ownership
semantics; no wall-clock speedup ratio is claimed from these regressions.
Both GTK releases were rebuilt after production code stabilized; their version
probes passed and all dynamic libraries resolved.

## Management upgrade validation (2026-10-25)

The Agent domain suite passed **264 tests** with the existing
host-dependent cgroup test excluded. New coverage restores hidden tasks to their
recorded states, refuses restore without one, keeps hidden tasks out of
attention counts, round-trips `archived_from` (including older records without
it), compares direct attention counts with dashboard projections and checks
their summaries, reports approval waits as slow-poll phases, and verifies that
unchanged views are shared while changed views get a new revision. Strict
all-target Clippy, rustdoc and formatting checks passed.
