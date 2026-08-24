# Engineering handoff

Updated: 2026-08-24

This baseline centralizes bounded AI transport, strict Agent restoration,
process-group lifecycle control, private atomic persistence, environment capture,
history retention, command review, and terminal helper primitives for all jterms.
It now also decodes persisted conversations under their own budgets, offers an
atomic claim/consume primitive for Agent snapshots, and vendors a fail-closed jsh
installer. Version 0.2 now pins jagent 0.7 and exposes its protocol-bound
request/response path, while preserving the serialize-only transcript boundary
and keeping non-streaming provider responses byte-oriented until their
canonical gate.

## 2026-08-22 ten-round AI configuration hardening

1. `AiClient::new` validates exact model bytes instead of trimming them.
2. Base URLs are likewise validated exactly; only endpoint trailing slashes
   are normalized after validation.
3. Both ordinary-chat and Agent request boundaries revalidate every mutable
   public client field, including token and temperature limits.
4. `AiClient` Debug reports lengths/presence and never raw model, URL, or key.
5. Environment model/base values default only when absent; the legacy
   `send_blocking` base-URL path also preserves explicit empty/padded values.
6. Explicit malformed `AI_MAX_TOKENS` values are errors instead of defaults.
7. Explicit malformed, non-finite, or out-of-range `AI_TEMPERATURE` values are errors.
8. Credential-path overrides and configured paths preserve exact non-empty
   text; surrounding spaces are rejected rather than retargeting file I/O,
   while `~/` expansion requires a safe absolute UTF-8 `HOME` and rechecks the
   final expanded byte ceiling.
9. `AiSettings` Debug does not echo provider/model/URL/path contents.
10. `display_name` remains one-line and byte-bounded after public-field mutation.

## 2026-08-22 twenty-round AI origin and authority hardening

1. AI base URLs now require an exact lower-case transport scheme instead of
   accepting a look-alike or normalizing caller-controlled text.
2. One authority splitter validates the complete host/port shape before any
   provider endpoint is appended.
3. Bracketed IPv6 authorities are accepted only when the literal parses and
   the closing bracket is followed by either nothing or one valid port.
4. Port text must be canonical decimal digits that fit in `u16`.
5. Port zero is refused at both AI-origin and clickable-link boundaries.
6. DNS hosts are ASCII labels with the conservative letter/digit/hyphen
   grammar and 63-byte per-label limit.
7. Ordinary IPv4 hosts must parse canonically rather than relying on a URL
   handler's legacy-number interpretation.
8. IPv6 literals are parsed as addresses rather than accepted as colon-shaped
   text.
9. Percent-encoded hosts fail closed before transport or opener resolution.
10. Unicode host text must be supplied as explicit ASCII punycode.
11. Empty, leading/trailing-hyphen, and trailing-dot DNS labels are rejected.
12. Hex, one-part, and shortened legacy IPv4 aliases remain rejected.
13. Cleartext AI transport is limited to loopback hosts; remote origins need
    HTTPS.
14. The complete `127.0.0.0/8` block is recognized as loopback, rather than a
    single hard-coded address.
15. Canonical IPv6 `::1` is recognized as loopback.
16. `localhost` recognition is ASCII-case-insensitive while the scheme remains
    exact.
17. A valid base path is preserved verbatim (apart from the established final
    slash normalization) for provider endpoint construction.
18. Construction and both mutable-client request boundaries call the same
    configuration validator, so post-construction mutation cannot bypass it.
19. Invalid-origin errors describe only the contract and never echo the URL,
    hostname, credential, query, or path supplied by the caller.
20. Table-driven regressions cover HTTPS DNS/IP origins, every supported local
    provider shape, malformed authorities, ambiguous hosts, and opener parity.

## Completed since the previous handoff

- The jagent pin advances to `fcb9768`. Ordinary blocking/streaming AI
  requests now re-run jagent's complete URL/header/unique-top-level-JSON/body
  postcondition immediately before curl, while builders retain the same
  compatibility surface. Validated clear-text loopback requests additionally
  emit curl's explicit no-proxy policy, so inherited proxy settings cannot
  redirect local provider credentials off-host; HTTPS keeps the user's proxy
  configuration. jagent also turns a second post-preparation history omission
  into a release-mode error, so the exact dependency consumed here now carries
  the same upstream guard in optimized builds.
- OSC 133 command metadata now preserves jsh's percent-encoded structural
  newline/tab bytes instead of discarding a valid multiline command. Repeated
  aliases for id/command/cwd/duration fail closed rather than using last-wins
  correlation. Execution-journal readers accept the pre-rename
  `rsh_execution_version` alias just as jsh does, and the v1 schema ceilings
  are public constants with jsh-shaped lifecycle fixtures pinning both ends.
- `agent` now re-exports jagent 0.7's `AgentRequestSpec`,
  `PreparedAgentRequest`, `AgentResponse`/`AgentStream`, and `AgentProtocol`;
  its hardened session wrapper accepts the bound response directly and exposes
  rejection feedback plus execution-failure observation. `AiClient` prepares a
  protocol-matched Agent request without routing review-first traffic through
  the ordinary chat builder. Snapshot auditing accepts the exact 0.6 and 0.7
  proposal-bound unknown-execution notes so a dependency upgrade does not make
  a legitimate in-flight restore unreadable.
- The earlier jagent pin advanced to `6aa3136`, and `jterm_core::agent` now re-exports
  `AgentCapabilities`, `AgentDelivery`, `CapabilityError`,
  `agent_capabilities`, the `AgentProvider` alias, and the version/wire-size
  constants. A facade-level negotiation test pins strict token parsing and
  provider/peer intersection so terminal consumers do not need a second direct
  jagent API surface.
- The coordinated jagent baseline keeps compatibility-first v1 emission, adds
  peer-aware v2 exact protocol/delivery modes with strict v1 decoding, and
  provides `CommandExecutionOutcome`. The core facade re-exports that upstream
  type and delegates typed observation directly, so failure paths no longer
  need a synthetic exit code or a duplicate compatibility enum.
- AI credentials now have one exact-value policy at environment/client,
  settings-write, credential-file-read, ordinary-chat, and Agent-request
  boundaries: non-empty visible ASCII with no whitespace. Credential files may
  normalize one final LF or CRLF only; stored and transported key bytes remain
  exact, and rejection errors never include credential material.
- `block_contract` keeps outcome and lifecycle evidence orthogonal through
  `CompletionProvenance`, `BlockLifecycleHealth`, and `assess_lifecycle`.
  Exhaustive tests pin shell-confirmed, journal-recovered, boundary-inferred,
  missing-start, and incomplete cases without inventing exit codes. Both enums
  carry dependency-free `schema_name()` methods so frontends can re-export the
  shared types without breaking their established public call surface.

- `src/parser.rs` is unified with forge's stricter terminal parser, retiring
  core's lenient control-string recovery. APC, DCS, PM/SOS, and every
  oversized discard state now terminate on ST only (BEL stays payload or a
  discarded byte), while OSC and OscDiscard keep accepting BEL per xterm
  convention. An ESC followed by a non-ST byte inside any control string
  aborts the partial string without emitting its payload — an aborted OSC 133
  can no longer forge prompt marks — and the ESC + final byte is reinterpreted
  as a fresh sequence through one shared `reprocess_escape_final!` path. RIS
  (`ESC c`, including when it aborts a control string) and a strict all-digit
  `CSI 3 J` (never `?3J`, `3;0J`, `3:0J`, or `3 J`) are pre-feed coalescing
  barriers: pending bytes flush, the `HardReset`/`EraseScrollback` event
  fires, and the raw sequence follows as its own immediate `Bytes` event;
  RIS also resets the parser's private-mode snooping (bracketed paste, mouse
  mode/encoding, focus events). OSC 7771 surfaces
  `ParserEvent::AgentIntegrationReady` only for an exact 32-hex-digit token
  and never passes through to VTE. SOS (`ESC X`) joins PM (`ESC ^`) in the
  discard-until-ST `Ignore` state.

- `ParserEvent::EraseDisplay` extends the same pre-feed barrier contract to
  ordinary all-digit `CSI 2 J` (including zero-padded `CSI 02 J`). The parser
  now preserves and recognizes `ESC [`, raw C1 `9B`, and UTF-8 U+009B
  (`C2 9B`) introducers for both ED2 and ED3. It flushes earlier bytes, emits
  the semantic event, then emits the exact control bytes before any suffix
  semantics. A three-byte Ground suffix window distinguishes C1 from `9B`
  continuation bytes inside ordinary two-, three-, and four-byte UTF-8 text
  without abandoning the bulk-copy fast path. Private, compound, colon,
  intermediate, overflow, wrong-final, and control-string lookalikes remain
  ordinary payload/pass-through and never gain the event; exhaustive split
  tests pin raw bytes and event ordering across every introducer form.

- Three forge-only additions are upstreamed so forge can delete its diverged
  local copies. `review_input::is_visual_spoofing_character` now keeps the
  unassigned specials `FFF0..=FFF8` and the entire supplementary tag plane
  `E0000..=E0FFF` whole instead of the enumerated tags, so future format
  assignments fail closed without a release lag (interlinear annotation
  anchors and Egyptian layout controls stay allowed). The same module gains
  the public display sanitizers `safe_inline_display` and
  `safe_multiline_display`, which replace controls and spoofing characters
  with U+FFFD (the multiline form preserves `\n`/`\t`) and truncate to
  `max_bytes` on a char boundary with a `…` marker. `pty_input` gains
  `AdmittedInput` and `admitted_input`, the single-pass editor-semantics
  classifier for sanitized outgoing chunks: framing markers only set/clear the
  frame state and are omitted from `editor_bytes`, CR/LF outside a frame
  submits the line (a CRLF/LFCR pair consumed silently), and bytes after a
  real submission (except 0x03/0x04) mark `input_after_submission`;
  `taints_editor` keeps the prompt fail-closed on framing-only writes.
  `execution_journal`'s private `enabled()` is promoted to the public
  `output_capture_enabled`, with `submit`, `flush`, and history loading
  calling it directly.

- `src/notify.rs` now sends through the `src/helper.rs` boundary instead of
  the older `host::helper_command` plus `host::command_status_with_timeout`
  route, retiring the second, differently-hardened notification path.
  `helper::notify_send` keeps its `(title, body)` shape for ember and frost;
  the new `helper::notify_send_with` carries caller-supplied options
  (`--app-name`, `--icon`, `--urgency`, `--expire-time`) ahead of the `--`
  guard, so core's own toasts get the same fixed-candidate trusted
  resolution, process-group ownership, concurrent byte-capped drains, one
  deadline, and synchronous reap. Queueing, truncation, and sanitisation in
  `notify.rs` are unchanged, and `host::command_status_with_timeout` stays
  for the Flatpak cwd/availability probes that still use it.

- `src/command_history.rs` gains `read_recent_with_status`, upstreamed from
  forge's local copy: the same bounded newest-first tail read plus a
  `tail_truncated` flag for the case where older bytes fell outside the 4 MiB
  window, so consumers never describe a short result as the complete history.
  `read_recent` is now a compatibility wrapper over it.

- `src/bounded_json.rs` holds the two schema-independent pieces of the
  RawValue bounded-decoder pattern ember and frost both carry in their
  session persistence: `TextBudget` (one cumulative checked-sub text budget
  per snapshot decode) and `DeferredRawField` (a borrowed `&RawValue` map
  field with duplicate tracking, so nested payloads are never cloned per
  ancestor and an explicit `null` still counts as present). The apps' repair
  counters and field enums stay app-side; they are the schema. Enabling
  serde_json's `raw_value` feature was the only manifest change.
- `src/command_history.rs` gains `prepare_path`, the pre-flight frost carried
  locally as `prepare_command_history_path` while the old pin opened history
  unsafely: the immediate parent must be owned by this user and never
  group/other writable (stricter than the append path's sticky-namespace
  allowance), a missing parent is created 0700 for writers only, and existing
  history/lock entries are descriptor-checked under `O_NOFOLLOW`/`O_NONBLOCK`
  with writers tightening a lax mode to 0600 and readers rejecting it.
- `src/link.rs` sinks the family's single opener policy: `is_openable_url`
  admits only an absolute HTTP(S) URL (case-insensitive scheme) with a
  non-empty, userinfo-free authority, at most `MAX_OPENABLE_URL_BYTES` (2 KiB),
  and no whitespace, control, backslash, or visually ambiguous characters.
  Authority parsing now also requires a valid bracketed IPv6/numeric-port
  shape plus a conservative ASCII DNS or valid IP host; encoded, Unicode,
  empty-label, numeric-alias, invalid-port, and parser-dependent spellings fail
  closed before a desktop opener sees them.
  frost's `link::is_openable_url` and ember's
  `terminal::is_supported_hyperlink_uri` were equivalent copies; both now
  delegate here, and the spoof check rides on `review_input`.
- `src/helper.rs` sinks the trusted-helper boundary ember and frost carried as
  near-identical local copies: `TrustedHelper` resolves a named helper from
  fixed absolute system candidates through `trusted_system_executable`
  (canonical target, every ancestor root- or self-owned, never group/other
  writable, a non-root user's own writable component refused), and `run`
  executes it with the child PATH clamped to `/usr/bin:/bin`.
  `bounded_command_output` captures stdout and stderr concurrently under
  independent byte caps and one absolute deadline on top of
  `supervised::SupervisedChild`, so the WNOWAIT exit observation, whole-group
  SIGKILL, and synchronous reap share the one audited implementation. The
  family's `fc_list`, `fc_match`, and `notify_send` entry points carry the
  previously duplicated caps and deadlines.
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
- `src/agent.rs` restore validation regains forge's stricter snapshot audit,
  ported from forge's removed `audit_agent_snapshot` that core's earlier
  adoption dropped. The pre-restore audit now requires the pending proposal to
  be the transcript's final turn (a "hidden" or "covered" pending can no
  longer split the reviewed card from the authorizable action), checks
  `turns_used` against the retained transcript's model-action and
  protocol-error counts (an exact range while untruncated, a lower bound once
  truncated), matches every state against the final turn's shape and budget
  exactly as the live transitions produce it (`Completed` only on a final
  assistant message, `Ready` never on a bare observation), and requires every
  approved proposal's fate to be recorded: an observation, the
  `AwaitingObservation` state, or — the one adaptation from forge's rule,
  which never restored an in-flight execution at all — the explicit
  unknown-result note jagent's restore normalization appends immediately after
  the proposal. The claim read moved from `read_bounded` to
  `read_bounded_private`, so a claimed snapshot with any group/other
  permission bits (e.g. 0640) is quarantined as tampering instead of restored.
  Forge's removed adversarial tests (hidden, covered, unobserved-approved,
  wrong-state, wrong-counter) are ported into core's suite, alongside a test
  that snapshots from every reachable live state/final-turn combination — the
  shapes anvil's jagent-produced sessions actually persist — still restore.
  `src/parser.rs` pins three reviewed gaps: a u32-saturating ED3 parameter
  (`CSI 42949672963 J`) is never erase-scrollback, an OSC aborted by
  `ESC BEL` drops its payload (no forged OSC 133 mark) and passes the raw
  bytes through, and the oversized APC/DCS discard states abort and reprocess
  a non-ST escape exactly like the OSC discard state.

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
