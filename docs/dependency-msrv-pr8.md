# PR #8: preserve Rust 1.86

The manifest's Rust 1.86 support remains unchanged. This is a dependency
compatibility repair, not an MSRV upgrade.

## Evidence and reuse

Original PR #8 head: `8e0fa39d6c2a20859310e3265432fbb3cf4d9507`.
`cargo +1.86.0 check --locked --all-targets --all-features` fails before
compilation: parking_lot 0.13.0, parking_lot_core 0.10.0 and lock_api 0.5.0
require Rust 1.95; uuid 1.29.0 requires Rust 1.89.
The unmodified PR compiles on Rust 1.99.1 with the same check command.

PR #7 head: `3f508c076b83586beafcf52c5b2fd217e9810120`.
[CI run 40052286778](https://github.com/beamiter/jterm_core/actions/runs/40052286778)
has successful stable tests, real Vim workflow, format/Clippy/docs, but its
Rust 1.86 job fails because uuid 1.28.0 requires 1.89. The separate pixel-input
workflow failed, so this is not evidence of an entirely green PR.
No separate unpublished compatibility test patch was recovered. Existing
zstd/session archive, LRU, directory and workflow tests are reused rather than
duplicated. Historical results do not substitute for the revised lockfile tests.

## Dependency decision

- Retain parking_lot 0.12.5 / parking_lot_core 0.9.12 / lock_api 0.4.14.
- Constrain uuid to `~1.26.0`, lock 1.26.1 (declared MSRV 1.85).
  The registry metadata puts the Rust 1.89 increase at uuid 1.27.0.
- Retain the other seven proposed upgrades, including zstd 0.14.3,
  lru 0.19.0 and dirs 7.0.0.
- Exclude parking_lot >=0.13 and uuid >=1.27 from routine Dependabot updates.
  Revisit these exclusions only with an explicit MSRV decision.
- Extend the existing stable/1.86 CI matrix to check and test all features
  and all targets with `--locked`.

## Lock and condition-variable audit

The project uses parking_lot Mutex, MutexGuard and Condvar in Agent drivers,
shared snapshots, Vim state/render watches, and the background workflow queue.
No custom raw lock implementations, recursive RwLock calls, removed const
constructors or explicit fair-unlock calls were found in these usages.
The original 0.13 build demonstrates source compatibility for these calls,
not behavioral equivalence or starvation freedom.

Upstream 0.13 changes raw guards to per-acquisition state, consumes that state
on unlock, and implements Condvar through lock_api's generic wrapper. Its
parking_lot Condvar still promises no spurious wakeups; project waits also
check predicates in loops. Cancellation/close state and notification ordering
must continue to avoid lost wakeups. Guard trait bounds and raw unwind rules
also change. Most importantly, automatic eventual fairness was removed and
contended wakeups are deferred until after unlock. A future upgrade needs
separate contention/tail-latency assessment even after the MSRV is raised.

New focused tests exercise the actual ViewCell with four contending writers
and snapshot readers (revision/content pairing and replacement identity), and
1,000 blocking render publication/acknowledgement cycles followed by close.
Existing Agent session claim, restore, task restart and worktree recovery
regressions are retained. These tests do not establish a fairness guarantee.

## Validation

- Revised Rust 1.86 all-target/all-feature locked check: passed.
- Rust 1.99.1 strict Clippy (`--locked --all-targets --all-features -- -D warnings`): passed.
- Formatting and `git diff --check`: passed.
- Rust 1.86 `test --locked --all-targets --all-features`: library suite
  1,892 passed, 11 failed, 5 ignored; Cargo stopped before remaining target tests.
- Focused Rust 1.86 Agent recovery (`agent::tests`): 43 passed.
- Focused Agent task suite: 265 passed, 1 failed (cgroup inspection unavailable).
- Render-watch suite: 12 passed, including the new 1,000-cycle test.
- Background workflow queue suite: 5 passed.

The 11 full-suite failures are listed below. Their diagnostics involve process
visibility, unavailable cgroup inspection, trust checks and denied shell
operations in this execution environment. They have not been classified as
pre-existing by a paired baseline run; they remain a merge gate, not waived
successes. Vim/minisign were unavailable and installation was restricted;
no new real-Vim or desktop acceptance result is claimed.

```text
agent_task::drivers::codex_app_server::tests::cgroup_scope_kills_term_ignoring_setsid_descendant_before_reap
ai::tests::captured_post_reap_cancel_and_deadline_handle_a_detached_pipe_holder
ai::tests::streaming_post_reap_cancel_and_deadline_handle_a_detached_pipe_holder
command_correction::tests::neither_helper_strategy_resolves_from_a_writable_namespace
git_meta::tests::status_probe_never_runs_repository_configured_programs
helper::tests::automatic_helpers_are_canonical_absolute_system_programs
process::lifecycle_tests::final_drain_kills_a_background_member_of_the_child_session
process::lifecycle_tests::the_process_group_preset_never_sweeps_the_session
process::tests::process_cwd_preserves_utf8_and_refuses_lossy_paths
process::tests::self_probes_read_this_process
vim9::shell::tests::shell_environment_is_child_local_durable_and_does_not_duplicate_path
```

Decision: publish the repair for review, do not merge until the revised head
passes CI on an appropriate runner (including Rust 1.86 and real-Vim checks).

Source: [PR #8 and upstream release notes](https://github.com/beamiter/jterm_core/pull/8).
