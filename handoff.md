# Engineering handoff

Updated: 2026-08-15

This baseline centralizes bounded AI transport, strict Agent restoration,
process-group lifecycle control, private atomic persistence, environment capture,
history retention, command review, and terminal helper primitives for all jterms.
It now also decodes persisted conversations under their own budgets, offers an
atomic claim/consume primitive for Agent snapshots, and vendors a fail-closed jsh
installer. Version 0.2 adopts jagent 0.6's serialize-only transcript boundary,
closes the same owning-string bypass for core AI turns, and keeps non-streaming
provider responses byte-oriented until their canonical gate.

## Completed since the previous handoff

- `src/supervised.rs` is now public API: `SupervisedChild` is the opaque
  helper-process runner the frontends migrate onto (forge's trusted-helper,
  Git-metadata, jsh-installer, and command-correction runners, plus anvil's
  command-correction runner). The struct keeps its `Child` private — callers
  take standard streams, observe the root with `root_has_exited`, and finish
  with `reap_after_group_kill`; `spawn` rejects auto-reaping SIGCHLD
  dispositions with `Unsupported`. Test-only helpers stay crate-private.
- `src/snapshot_file.rs` gains `read_bounded_private`, the 0600-or-nothing
  reader anvil and forge previously carried as local copies for organism
  memory: on Unix it rejects any group/other permission bits (any owner-only
  mode is accepted) on the same open descriptor that is read, then shares the
  bounded-read body with `read_bounded` through `read_bounded_file`.
- `src/atomic_file.rs::temp_file_name` is public so frontends can assert their
  snapshot-directory scans never read back an in-flight temp name without
  copying the formula.

## Previously completed

- `src/supervised.rs` now owns every short-lived core helper used by host
  probes, Git metadata, the jsh update check, and blocking or streaming AI
  transport. On Unix it keeps the root waitable with `waitid(..., WNOWAIT)`,
  clears the fresh process group before consuming the root status, and
  synchronously reaps on success, timeout, cancellation, output overflow, and
  early-return paths. The first group signal permanently disarms the guard, so
  an `ECHILD` or a second cleanup cannot target a recycled PGID; host, Git, jsh,
  and AI each carry one absolute deadline across spawn and collection.
  Auto-reaping `SIGCHLD` dispositions are rejected; a benign custom handler is
  allowed, but no external waiter may consume a supervised child's status.
  After a logical deadline the final synchronous kernel wait can still exceed
  wall time for a task stuck in uninterruptible sleep.

- `src/block_contract.rs` establishes the renderer- and serialization-free
  completed-block outcome shared by all four terminals. `classify_completed`
  gives an absent/blank frontend-resolved command precedence as `Background`,
  distinguishes an explicitly reported zero (`Success`) from non-zero
  (`Failed`) and from no reported status (`Unknown`), and preserves the exact
  observed status through `reported_exit_code`. Hostile command text and a
  property-style status matrix pin the boundary without importing frontend or
  persistence types.

- `src/jsh_remote.rs` turns a remote-host description into argv that runs the
  newly vendored `scripts/jsh-remote.sh`, which places a verified static jsh on
  a destination that has none for the life of a session and removes it after.
  `Deploy` is `Off`/`Persist`/`Incognito`; `parse` returns `None` for anything it
  does not recognise so a caller rejects a typo instead of downgrading
  `incognito` — the modes differ in whether the destination's `$HOME` is written
  to. `publish_launcher` and `launch_argv_with_script` are separate from
  `launch_argv` so an app can assert argument order without publishing, and can
  fall back to plain ssh when only publication fails. anvil and forge both
  consume it through a new `deploy` key on `[[remote_hosts]]`.
- `src/vendored_script.rs` holds the "publish an embedded script so it can be
  executed" logic that `jsh_install` used to own privately: private directory,
  `O_NOFOLLOW`, regular-file/owner/link checks, byte-comparison before reuse, and
  atomic publication. `jsh_install` now delegates to it, so the installer and the
  remote launcher cannot drift apart on any of those properties.

- `src/ai/conversation.rs` decodes both schema versions through
  `DeserializeSeed`/`Visitor` implementations that stop before chat 51 and turn
  101, reject unknown and duplicate fields, and charge per-field plus cumulative
  title, turn, draft, and context budgets while constructing. `ChatSnapshot` and
  `ConversationSnapshot` no longer implement `Deserialize`, so `from_json` is the
  only wire path, and every public `ConversationSnapshotError` category is
  preserved — a budget records its own reason instead of surfacing as
  `InvalidJson`.
- `src/agent.rs` gained `claim_session_file`, a one-winner claim/consume
  primitive built on `snapshot_file::claim_exclusive`. Linux/Android use one
  raw `renameat2(RENAME_NOREPLACE)` syscall and macOS uses
  `renamex_np(RENAME_EXCL)`, preserving the existing `.claimed-*` name without
  the transient link-count and process-crash window of hard-link/unlink. A
  pre-existing target is never overwritten; platforms without an equivalent
  primitive fail closed. Evidence that cannot become a session is quarantined
  at the claim path rather than deleted, while ordinary corrupt-snapshot
  quarantine retains its portable evidence-preserving fallback.
- `src/agent.rs::try_claim_session_file` now distinguishes absence or a lost
  claim race (`Ok(Vacant)`) from every other acquisition failure (`io::Error`).
  It never falls back to reading the public name, while successful claims whose
  evidence cannot restore remain `Ok(Quarantined)`. The historical
  `claim_session_file` delegates to it and preserves best-effort compatibility
  by logging and collapsing typed errors to `Vacant`.
- `src/agent.rs::validate_snapshot` now audits jagent's bounded immutable
  snapshot accessors directly. The former `to_json` plus ordinary
  `SnapshotInspection { transcript: Vec<Turn>, .. }` decode is gone, while all
  family-level proposal, observation, command, counter, and active-state checks
  remain unchanged.
- Non-streaming AI success bodies remain raw bytes through curl collection and
  enter `parse_chat_response_full_bytes`, which applies jagent's 1 MiB ceiling
  before constructing a JSON value. Non-2xx bodies may still be collected as
  bounded evidence, but only their first 2 KiB can enter the diagnostic JSON
  parser; streamed responses retain their frame and cumulative limits.
- Blocking and streaming AI requests now consume jagent's `*_with_report`
  builders. Core retains its pre-allocation history window and carries that
  exact omission count into a complete system notice; any further omission by
  jagent is treated as an invariant violation and fails closed. Raw and
  redacted system instructions have a strict 64 KiB ceiling, and adding the
  optional separator plus complete notice uses checked arithmetic and rejects
  overflow instead of sampling safety instructions. Core's public `ai::Turn`
  is serialize-only, so persisted conversations can decode only through the
  bounded `ConversationSnapshot::from_json` path.
- `scripts/install-jsh.sh` is resynced from the hardened canonical jsh copy: a
  published SHA-256 is mandatory and format-checked, downloads are byte-bounded
  and HTTPS-only across redirects, version/target/base-URL grammars are validated
  before any path or URL is built, archive members are checked for links,
  traversal, and extra payload before extracting exactly the expected binary, and
  the update-check cache is private, symlink-safe, and atomically replaced.

## Remaining boundaries

### Finish Forge's stronger claim validation boundary

Anvil, ember, and frost now consume core's `SessionClaim::Restored` directly;
they do not run a second app restore/audit after core has consumed the claim.
Typed claim-acquisition failures are also complete in core through
`try_claim_session_file`. Forge alone retains a local dirfd/inode-bound
transaction because it enforces policy beyond core's path-based primitive. It
should keep that stronger transaction until core exposes a pre-retire
validation hook that can preserve Forge's late validation without losing
evidence. After that migration, decide whether to deprecate the public
best-effort read/remove pair and legacy claim wrapper so a new integration
cannot accidentally reintroduce a two-step restore race.

### Add a signed release manifest to the installer

The mandatory SHA-256 is same-origin: it proves the bytes match what the release
published, not who published them. A detached signature over the manifest,
verified against a key pinned in the installer, is the missing half. That needs a
release-side signing decision, so it is deliberately not approximated here.

## Release checks

```text
cargo fmt --all -- --check
cargo test --all-targets --all-features --no-fail-fast
cargo test --doc
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps
```

`process::lifecycle_tests::final_drain_kills_a_background_member_of_the_child_session`
is timing-sensitive and has been observed failing once under a loaded machine
while passing on every rerun; if it fails, rerun before investigating.
