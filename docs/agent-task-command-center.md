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
