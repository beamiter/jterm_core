# Agent Workspace v2 validation

Verified on 2026-11-02 with Rust 1.99.1 on Linux, using the locked dependency
graph, offline Cargo and one build job. Process/PTY/socket fixtures ran with
normal local process visibility; the restricted sandbox's isolated `/proc`
cannot attest the fixture child identities used by the ownership tests.

This record concerns the core changes committed with it: workspace protocol 2,
SQLite schema 5, shared presentation/query/actions, search/context/result
evidence, dependency intent, observed usage and read-only SSH observation.

## Completed gates

| Gate | Result | Scope |
| --- | --- | --- |
| Related core unit and fixture tests | 69 passed, 0 failed | All 46 `agent_workspace` tests; 5 original task-query tests; 13 Claude parser/process fixtures; 3 usage reducer tests; 1 Codex usage/correlation test; 1 worktree evidence test |
| Hosted workspace integration | 28 passed, 0 failed, 3 ignored | Real local PTY/socket/process fixtures; API lifetime and exact receipts; native handoff and ownership; validation/acceptance; v2 search/context/result evidence |
| SSH observation integration | 3 passed, 0 failed | Real stdio bridge through a local SSH stub; exact remote evidence; identity mismatch; disconnect without retry; mutation rejection; no absent-host startup |
| Final task presentation gate | 7 passed, 0 failed | All projection tests, including one new external-task regression; six previously passed tests rerun |

The initial related gate compiled the full library test target, containing
2017 tests at that point. It ran the 69 named tests below; **1948 other library
tests were not run** in that gate. The later presentation regression increased
the compiled target to 2018 tests. Earlier test totals from prior releases are
not new verification.

```bash
cargo test --offline --locked --lib -- \
  agent_workspace:: \
  agent_task::usage:: \
  agent_task::query:: \
  agent_task::worktree::review:: \
  agent_task::drivers::claude_stream_json:: \
  agent_task::drivers::codex_app_server::tests::usage_notifications \
  --test-threads=1
```

The initial related unit gate includes the fixed index-trigger conflict, immutable
search anchors, SHA-256 document revisions, pinned-row selection in legacy
Refresh, queue CAS/cycles/deletion, recorded/current separation and partial
overview coverage. Provider usage fixtures prove that repeated Codex thread
totals replace previous totals, unrelated thread events are rejected, and two
Claude turns with duplicate result records contribute only once each.

The final presentation-only gate ran after isolated GTK testing found that an
external task without current task facts displayed the generic idle label
"Ready to resume". Such tasks now display "Task details unavailable"; the
separate idle task-runtime label is "Idle". The regression also checks unknown
capabilities, read-only navigation, and preservation of observed-running,
owner-conflict, known-created and archived precedence. No runtime state or
control permission changed.

```bash
cargo test --offline --locked --lib \
  agent_workspace::projection::tests:: -- --test-threads=1
```

That final gate passed all 7 projection tests, with 2011 other library tests
filtered out. It did not rerun the full earlier related or integration suites.

## Integration gates

The integration targets were built with the result shell comparison and
fixture path portability change, before the final presentation-only adjustment
recorded above:

```bash
cargo test --offline --locked --no-run \
  --test agent_workspace_host \
  --test agent_workspace_ssh
```

The two test executables and the built `jterm-sessiond` were copied to an
accessible temporary directory, with SHA-256 equality checked against the
original binaries. Both suites then ran serially as UID/GID **65534**, with no
supplementary groups. `JTERM_TEST_HOST_EXECUTABLE` selected that exact copied
helper; its default remains Cargo's built helper path. All state and fake
provider files were isolated temporary fixtures. The host suite completed in
31.89 seconds and the SSH suite in 0.55 seconds; both returned exit code 0.

The unprivileged execution is an environment adaptation, not a relaxed helper
policy. The initial UID 0 host run passed 21 tests and failed 7 API tests
because the existing automatic-helper policy correctly refused a system curl
and ancestor paths writable by that UID. A first unprivileged attempt then
hit the root-owned checkout's private ancestor directory. Copying the exact
binaries and using the test-only helper path override resolved both constraints
without changing workspace permissions or production executable validation.
The final **28 / 0 / 3** result above replaces that failed environment run.

The three ignored host tests are internal child-process entrypoints invoked by
their parent lifecycle tests. They are not counted as independent passed
tests. The initial targeted gates total **100 distinct passed tests**. The new
presentation regression raises the combined distinct total to **101**; its six
rerun projection tests and repeated diagnostic runs are not added again.

## Evidence boundaries

All data is fixture data in temporary directories. Provider processes are fake
executables; the SSH executable is a local stub that exercises the real stdio
bridge and an isolated host. Tests do not read real provider transcripts or
credentials, contact real models, or connect to the user's SSH infrastructure.

Core validation does not establish desktop X11/Wayland, IME, screen-reader,
focus or rendering behavior. Those require separate frontend verification.
The SSH feature observes an already-running remote host through explicit
configuration; it does not implement remote task execution. DLC execution and
cross-host task migration remain unsupported. No usage or cost is estimated
when the provider did not report it.

Automatic worktree validation evidence currently refuses repositories with
submodules. Catalog sorting is verified separately from persistent body-search
cursor pagination; the latter does not claim the catalog's relevance/recent/
attention sort modes.
