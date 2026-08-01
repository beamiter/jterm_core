# Engineering handoff

Updated: 2026-08-01

This baseline centralizes bounded AI transport, strict Agent restoration,
process-group lifecycle control, private atomic persistence, environment capture,
history retention, command review, and terminal helper primitives for all jterms.

## Remaining boundaries

### Decode persisted conversations while enforcing budgets

`src/ai/conversation.rs` applies an 8 MiB encoded cap and strict semantic validation,
but v1/v2 snapshots still use ordinary Serde collection deserialization. Introduce
schema-specific `DeserializeSeed`/`Visitor` decoding that stops before chat 51 and
turn 101, rejects duplicate/unknown fields, and charges per-field plus cumulative
title, turn, draft, and context budgets during construction. Preserve both schema
versions and current public errors.

### Make Agent snapshot consumption allocation-aware and one-shot

`src/agent.rs` validates proposal ordering, pending state, commands, and counters
before restoring a session. The upstream JSON decoder remains bounded only by its
encoded byte cap; complete the visitor work tracked in `jagent/handoff.md` and keep
the local semantic audit. Also add an atomic claim/consume primitive so callers do
not implement restore as a racy `read_snapshot_file` followed by
`remove_snapshot_file`.

### Harden the embedded jsh installer trust chain

The embedded installer has bounded process handling, but the shell script still
needs a release-grade trust pass:

- Require and validate a published SHA-256 instead of continuing when it is absent.
- Validate version, target, and base URL before constructing paths or URLs.
- Reject unsafe archive members and links; extract only the expected binary.
- Make installer cache creation private, symlink-safe, and atomically replaceable.

Keep explicit source builds separate from the automatic release-install path.

## Release checks

```text
cargo fmt --all -- --check
cargo test --locked --all-targets --all-features --no-fail-fast
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo doc --locked --all-features --no-deps
```
