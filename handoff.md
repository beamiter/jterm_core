# Engineering handoff

Updated: 2026-08-01

This baseline centralizes bounded AI transport, strict Agent restoration,
process-group lifecycle control, private atomic persistence, environment capture,
history retention, command review, and terminal helper primitives for all jterms.
It now also decodes persisted conversations under their own budgets, offers an
atomic claim/consume primitive for Agent snapshots, and vendors a fail-closed jsh
installer.

## Completed since the previous handoff

- `src/ai/conversation.rs` decodes both schema versions through
  `DeserializeSeed`/`Visitor` implementations that stop before chat 51 and turn
  101, reject unknown and duplicate fields, and charge per-field plus cumulative
  title, turn, draft, and context budgets while constructing. `ChatSnapshot` and
  `ConversationSnapshot` no longer implement `Deserialize`, so `from_json` is the
  only wire path, and every public `ConversationSnapshotError` category is
  preserved — a budget records its own reason instead of surfacing as
  `InvalidJson`.
- `src/agent.rs` gained `claim_session_file`, a one-winner claim/consume
  primitive built on `snapshot_file::claim_exclusive`. The snapshot is moved to a
  private name before it is read, so two simultaneous openers cannot both resume
  it and a restore is no longer a racy read-then-remove. Evidence that cannot
  become a session is quarantined at the claim path rather than deleted.
- `scripts/install-jsh.sh` is resynced from the hardened canonical jsh copy: a
  published SHA-256 is mandatory and format-checked, downloads are byte-bounded
  and HTTPS-only across redirects, version/target/base-URL grammars are validated
  before any path or URL is built, archive members are checked for links,
  traversal, and extra payload before extracting exactly the expected binary, and
  the update-check cache is private, symlink-safe, and atomically replaced.

## Remaining boundaries

### Adopt the claim primitive in the apps

`claim_session_file` exists, but `read_snapshot_file` + `remove_snapshot_file`
remain public and are what jterm1..4 still call. Migrate each app (their handoffs
track this) and then decide whether the racy pair should be deprecated or
removed, so a future caller cannot reintroduce the two-step restore.

### Make Agent snapshot decoding one-shot end to end

`validate_snapshot` still re-encodes the snapshot and decodes a second
`SnapshotInspection` view to audit it. The upstream jagent now decodes snapshots
through bounded seeds, so once the pinned revision is advanced this layer should
audit the decoded value directly instead of paying for — and trusting — a second
serialization round trip.

### Add a signed release manifest to the installer

The mandatory SHA-256 is same-origin: it proves the bytes match what the release
published, not who published them. A detached signature over the manifest,
verified against a key pinned in the installer, is the missing half. That needs a
release-side signing decision, so it is deliberately not approximated here.

## Release checks

```text
cargo fmt --all -- --check
cargo test --locked --all-targets --all-features --no-fail-fast
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo doc --locked --all-features --no-deps
```

`process::lifecycle_tests::final_drain_kills_a_background_member_of_the_child_session`
is timing-sensitive and has been observed failing once under a loaded machine
while passing on every rerun; if it fails, rerun before investigating.
