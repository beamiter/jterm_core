# Engineering handoff

Updated: 2026-08-08

This baseline centralizes bounded AI transport, strict Agent restoration,
process-group lifecycle control, private atomic persistence, environment capture,
history retention, command review, and terminal helper primitives for all jterms.
It now also decodes persisted conversations under their own budgets, offers an
atomic claim/consume primitive for Agent snapshots, and vendors a fail-closed jsh
installer. Version 0.2 adopts jagent 0.6's serialize-only transcript boundary and
keeps non-streaming provider responses byte-oriented until their canonical gate.

## Completed since the previous handoff

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
- `scripts/install-jsh.sh` is resynced from the hardened canonical jsh copy: a
  published SHA-256 is mandatory and format-checked, downloads are byte-bounded
  and HTTPS-only across redirects, version/target/base-URL grammars are validated
  before any path or URL is built, archive members are checked for links,
  traversal, and extra payload before extracting exactly the expected binary, and
  the update-check cache is private, symlink-safe, and atomically replaced.

## Remaining boundaries

### Consolidate the app-local claim implementations

All four terminals claim Agent snapshots before production restore, but ember,
forge, and frost still carry separate implementations while anvil calls
`claim_session_file` directly. Forge's dirfd/inode-bound transaction still
enforces policy beyond core's path-based primitive. A consumer of core
`Restored` must use that session directly, not run a second app restore/audit
after core has already consumed the claim. Apps with policy that core does not
cover must keep their local transactional claim (including Forge for now) until
core exposes a pre-retire validation boundary. Then decide whether the public
read/remove pair should be deprecated and whether claim I/O failures need a
typed public outcome instead of the current logged `Vacant`, so a future
restore cannot reintroduce a two-step race, lose evidence after late validation,
or hide an unavailable platform primitive.

### Add a signed release manifest to the installer

The mandatory SHA-256 is same-origin: it proves the bytes match what the release
published, not who published them. A detached signature over the manifest,
verified against a key pinned in the installer, is the missing half. That needs a
release-side signing decision, so it is deliberately not approximated here.

## Release checks

```text
cargo fmt --all -- --check
cargo test --all-targets --all-features --no-fail-fast
cargo clippy --all-targets --all-features -- -D warnings
cargo doc --all-features --no-deps
```

`process::lifecycle_tests::final_drain_kills_a_background_member_of_the_child_session`
is timing-sensitive and has been observed failing once under a loaded machine
while passing on every rerun; if it fails, rerun before investigating.
