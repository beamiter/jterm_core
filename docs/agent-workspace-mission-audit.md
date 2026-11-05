# Mission MVP audit and verification boundary

Audited baseline: `f8566bb9cb6e9cca3258c8050c6dc4e9b96da8ff`, 2026-11-05.
This records the shared core, not a claim that all four frontend release gates
or the complete five-repository delivery demonstration have passed.

## Existing implementation

- `WorkspaceMission` contains stable identity, a revisioned acceptance goal,
  required environment, repository candidates and existing task/run references.
  Its five-repository preset requires core, Anvil, Forge, Ember and Frost.
- The existing session host is the sole SQLite writer. Schema 6 persists plans
  and dated reports; revision comparisons reject conflicting writes. Inspection
  uses bounded workers rather than blocking the host loop.
- `MissionWorkbench` provides asynchronous catalog, selection, save, inspect,
  candidate adoption and task-link operations. An uncertain write reads back
  the same identity instead of replaying the write.
- Candidate observations include baseline, HEAD, a tracked/staged/untracked
  fingerprint, dirty state, manifest/lock pins and a dated remote target read.
  Dirty candidates cannot be silently adopted. Target ancestry is checked only
  with existing local Git objects, with replacement refs and grafts disabled.
- Workspace validation binds the existing task incarnation, worktree,
  fingerprint, command and acceptance-contract time boundary. Check projections
  distinguish passed, failed, blocked, not run and stale. A later failed attempt
  cannot borrow an earlier pass.
- Integration is per repository. Committed, pushed, integrated and validated
  are independent gates; a partially integrated set is not a completed Mission.

## Corrections in this change

1. Dependency evidence now also reads both committed Cargo configuration names.
   A core patch/replacement, Cargo path override or source replacement cannot
   prove the compiled core merely by leaving a nominal Git SHA in Cargo.lock.
   Duplicate core packages, including a second path or unrelated Git source,
   are rejected. The underlying local snapshot remains available so the user
   can still inspect and select the candidate that needs correction.
2. Reopened reports and catalog refreshes preserve receipt references and dated
   observations but invalidate current delivery gates until a fresh inspection.
   A plan revision alone cannot prove the filesystem or remote ref stayed put.
   Reading does not overwrite the historical report stored in SQLite.
   A refresh also rejects older in-flight inspection replies. Report fields
   `pushed_observation` and `integration_observation` preserve exact remote
   refs, heads, observation times and candidate ancestry facts independently
   from the current boolean gates; legacy reports still deserialize.
3. When a linked validation is stale, the aggregate validated flag and missing
   checks now agree with the stale check label.

Wire protocol 2, schema 6 and existing frontend controller methods are unchanged.
The added report helper and observation configuration fields are additive.

## Remaining acceptance work

- A report is a dated observation, not a continuous filesystem watcher. While
  a view stays open, external edits need a new inspection. The refresh/reopen
  correction deliberately does not claim continuous invalidation.
- Candidate pin inspection is not full Cargo provenance. Ancestor/global Cargo
  configuration, ignored local configuration, environment overrides, custom
  commands and the dependency content used by a build require a controlled
  validation environment and corresponding evidence.
- The host currently produces the built-in `validation` receipt in environment
  `local`. The CI enum is a data-contract option, not an implemented trusted CI
  importer. Declaring arbitrary CI, GPU, Wayland or IME checks cannot produce a
  pass. Review, screenshot and richer environment receipt ingestion remain open.
- Needs-you currently exposes incomplete Mission/repository flags and counts.
  The existing task command center has specific approval, reply, validation,
  review and failure queues. A shared reasoned Mission queue with exact task,
  log and diff navigation still needs frontend integration and acceptance.
- Four native frontends must independently expose the same Mission state and
  preserve selection, drafts and existing task identities. Publishing the core
  does not complete those integrations.
- The final demo still needs one real core change, four exact frontend pins,
  matching final validation receipts, independent review, interrupted/reopened
  continuation and verified per-repository remote integration facts.
- V2.1 remains a separate stability gate. Physical display, real Chinese IME,
  live provider recovery and hardware/backend coverage must be recorded as
  passed, failed, blocked or not run for the final delivered versions.

No new daemon, provider identity, task scheduler, automatic push/merge executor,
remote execution service or model gateway is introduced by this change.

## Local verification (2026-11-05)

The measurements below describe the initial corrective tree published as
`6260d0c1ac99c544b19b4b6d0e4d1cb8e6111f10`.
The cloud execution container used official Rust 1.99.1. Its filesystem ownership
and restricted Unix socket/process facilities differ from a normal Linux desktop.

- `cargo fmt --all -- --check`: passed.
- `cargo test --locked --lib mission`: 47 passed, including the Mission model,
  controller refresh barrier, legacy-host read handling, backward-compatible
  receipt decoding and SQLite reopen tests. The filter also selects names
  containing "submission" or "admission"; this is not 47 distinct Mission tests.
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: passed.
- `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps`: passed.
- `cargo test --locked --all-targets --all-features`, with a fresh temporary
  directory: 2016 passed, 45 failed, 5 ignored in the library stage. Later target
  test executables were not reached after that stage failed.
- The unchanged `f8566bb` baseline in the same environment produced 2008 passed,
  42 failed, 5 ignored. All 42 baseline failures recur in the changed tree. The
  only additional failures are the three new Cargo configuration Git fixtures,
  which stop before their assertions because the system-Git trust guard rejects
  this container's user-owned root directory. Existing failures also include
  restricted Unix sockets/PTYs, unavailable systemd scope attachment and hardened
  executable probes. The production trust and confinement checks were not relaxed.

The full suite therefore has **not** passed locally. Normal Linux CI must execute
the Git fixtures and host integration gates for the published commit. These
results do not establish MSRV, native frontend, physical display or IME coverage.

Subsequent hardening keeps the runtime lease contract unchanged: queue and
Mission reopen tests use the existing isolated-process lease harness, because
parallel tests intentionally hold forked children before exec. All 17 store
tests and Clippy passed for that test-only change. Commit `8853247` then passed
both Rust stable and 1.86 CI suites and the Real Vim pixel workflow.

Candidate adoption now additionally requires an explicit successful inspection
of the current selected revision. Refresh, reads, selection and saves revoke
eligibility; older inspection replies cannot restore it. Historical observations
remain visible, but the shared `can_adopt_candidates()` gate and action reject
their reuse, dirty or missing observations and empty repository sets. The updated
Mission-filter test run passed all 49 tests, with Clippy, rustfmt and rustdoc
passing. This is still not a continuous filesystem lock or watcher.
