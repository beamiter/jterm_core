# jterm_core

`jterm_core` is the UI-independent foundation shared by anvil, ember,
frost, and forge. It contains terminal protocol, process, persistence,
AI/Agent, and policy code that must behave identically across GTK, egui, and
iced frontends.

The crate deliberately has no GUI-toolkit dependency. Frontends own widgets,
rendering, and event-loop integration; this crate owns byte-level protocols,
bounds, validation, and operating-system primitives.

Version 0.2 adopts jagent 0.7's protocol-aware request boundary. Agent system
prompt, Text/NativeTools schema, delivery mode, redaction report, response
decoder, and session ingestion can travel as one bound path, while historical
entry points remain compatible. Agent and conversation snapshots restore only
through allocation-aware bounded decoders; their owning-string transcript
values remain serialize-only. Non-streaming provider responses remain raw
bytes until jagent checks its 1 MiB envelope ceiling.

`jterm_core::agent` also exposes jagent's versioned capability contract:
`AgentCapabilities`, `AgentDelivery`, `CapabilityError`, `agent_capabilities`,
the provider alias `AgentProvider`, and the
`AGENT_CAPABILITIES_*`/`MAX_AGENT_CAPABILITIES_WIRE_BYTES` constants. Frontends
can therefore parse and negotiate a bounded peer token without depending on
jagent through a second public path or transporting credentials.
The facade also re-exports jagent's `CommandExecutionOutcome`, peer-aware v2
capability helpers, and `AgentSession::observe_execution`, so a frontend can
report a real exit separately from failed start, timeout, or cancellation
without inventing an exit code. Compatibility-first discovery still emits v1;
v2 is selected only for a peer that has already advertised v2 support.

## Shared surfaces

- OSC/CSI/DCS/APC parsing, Kitty graphics framing, character widths, themes,
  the family keybinding grammar, and the four-way completed-block outcome
  contract (background, success, failure, or unknown status). Completion
  provenance is tracked separately as shell-reported, journal-recovered,
  boundary-inferred, or unknown, with one renderer-neutral lifecycle-health
  mapping shared by every frontend. Both lifecycle enums expose stable,
  dependency-free `schema_name()` spellings so a frontend can re-export them
  without changing its existing diagnostic or JSON call surface. Strict
  ordinary-numeric `CSI 2 J` and `CSI 3 J` sequences surface pre-feed
  `EraseDisplay`/`EraseScrollback` barriers before their original bytes, for
  `ESC [`, raw C1 `9B`, and UTF-8 U+009B (`C2 9B`) introducers. A bounded
  Ground-state UTF-8 suffix classifier prevents an unrelated continuation
  byte `9B` inside normal Unicode text from becoming a control event, so
  renderers can invalidate row authority without rescanning arbitrary output.
  Raw iTerm2 `OSC 1337;CurrentDir` compatibility values reach the terminal only
  when the exact path is bounded and visually unambiguous; OSC 7 remains the
  canonical encoded cwd channel.
- PTY input guarding, review-only command insertion, child environments,
  process-group lifecycle management, and restorable-command quoting.
- Private atomic snapshots, command history, jsh execution journals, pane
  layouts, Git metadata, notifications, and host/Flatpak command routing. The
  journal v1 version and byte/event/record ceilings are public, and its reader
  keeps jsh's JSON-escaped multiline commands plus the pre-rename version alias
  so migrated shell history and live OSC metadata resolve identically. Every
  physical line, including future-version and unknown additive events, is
  charged before parsing so ignored extensions cannot bypass the event budget.
  A writer also checks that budget before emitting a separator or event, and an
  exact-limit journal is left byte-for-byte unchanged when an append is refused.
  Successful appends sync their data, and a newly created journal additionally
  syncs its parent directory. A failure before the first event byte removes a
  newly created empty pathname; after any byte becomes visible, write or
  durability-barrier failures report an unknown commit state and never roll
  back or retry the event internally.
  Its incremental counter treats an unterminated peer tail as one physical line;
  a later LF only terminates that line rather than charging it twice.
  A custom journal override must use a directory owned by the current user and
  not writable by group or other; shared namespaces such as `/tmp` are refused
  because the journal and its fixed `executions.lock` sidecar form one trust
  boundary. The journal file itself cannot use that reserved sidecar name
  (including case aliases on case-insensitive filesystems).
  `is_valid_jsh_execution_id` exposes the exact 1–192-byte ASCII token grammar
  that correlates jsh lifecycle and output events without narrowing generic
  OSC 133 identifiers used only in a terminal's in-memory timeline.
  A durable terminal Output additionally requires one complete OSC `C`
  capability containing the exact `session_id`, `id`, `seq`, and
  `started_at_ms` Start generation; old or incomplete marks remain valid UI
  boundaries but cannot reach the journal writer. The asynchronous writer
  validates that capability against the current authoritative Start while
  holding the journal's exclusive lock. A reset/restart, a Finish conflict
  tombstone, an existing Output slot, or an unterminated physical tail rejects
  the event before writing. An ordinary authoritative Finish does **not** close
  an empty Output slot: jsh emits OSC 133 `D` and appends its Finish in the next
  statement on the same thread, while a terminal only learns the command ended
  by parsing that `D`, so Start/Finish/Output is the normal order and closing
  the slot at Finish would reject every terminal contribution rather than only
  late ones. What makes the late Output safe is the lifecycle capability, which
  is re-checked field by field against the authoritative on-disk Start under the
  lock. If a complete Output becomes visible but its durability barrier fails,
  only one unique, exact physical-tail match with no Finish before or after it
  can retry the barriers; the event itself is never appended again.
  `is_valid_jsh_cwd` gives both channels one exact, nonempty, bounded, visually
  unambiguous cwd identity rule.
  Journal finish and output slots accept exact duplicate delivery idempotently;
  conflicting duplicates degrade only their own slot to unknown until a new
  authoritative start resets that execution id's lifecycle. Compaction carries
  the poison as an additive `conflict` tombstone that legacy v1 readers ignore
  to the same unknown result. Start authority follows physical event order, so
  a restart still clears both prior slots when its sequence or wall clock moves
  backwards. The bounded fold likewise evicts by physical Start ordinal;
  session history keeps that same physical chronology across clock or sequence
  resets, while timestamps remain untrusted display fields. Recursive
  duplicate-member validation runs before any event can mutate lifecycle state,
  including an invalid Start barrier. Unknown event kinds remain skippable for
  forward compatibility, but known v1 events reject extra members so an
  injected identity or session hint cannot alter lifecycle correlation.
  Decoded-key duplicates and simultaneous canonical/legacy version names are
  invalid envelopes, while future versions of known event kinds remain
  forward-compatible non-barriers and cannot select v1 lifecycle state.
  A recognized v1 start with a valid execution id retires that id's prior
  lifecycle before its remaining fields are decoded strictly, so an invalid
  replacement cannot redirect later finish/output events back to stale state;
  malformed, future-version, and unknown additive events remain non-barriers.
  OSC metadata that claims a command is truncated (or encodes that disclosure
  ambiguously) never enters the exact-command slot, even if a producer also
  supplies a partial command field.
  Command-end outcomes accept FinalTerm's positional status and the family's
  `exit`, `exit_code`, and `exit_status` aliases; repeated outcome slots degrade
  only that status to unknown while independent metadata survives.
  Numeric outcome, duration, and truncation-boolean aliases accept
  Ember-compatible surrounding whitespace without percent-decoding malformed
  values into authoritative metadata.
  `bounded_json::validate_no_duplicate_members` re-exports jagent's wire
  preflight and gives bounded credential, IPC, and persistence decoders one
  allocation-light recursive preflight, so escaped or plain duplicate object
  names cannot acquire parser-dependent meanings before typed deserialization.
  Callers enforce their raw byte ceiling first; the preflight also rejects
  serde_json's private RawValue escape key before a feature-unified `Value`
  decoder can reinterpret its string as unchecked JSON.
- The native ASCII organism: its simulation, attention model, and bounded
  repo-scoped long-term memory. Only structured counters, a short transition
  window, and a quantized life snapshot are stored; command text, output, and
  PIDs never enter the file, which is capped at 512 KiB with bounded day,
  observation, and pending-event ceilings. The memory path is caller-owned —
  core has no opinion about where an app keeps state, and a wrong default is
  indistinguishable from an organism that has never run. Durability is the
  app's: every consumer must implement `organism_memory::MemoryScheduler` and
  register it once at startup with `init_scheduler`, beside `identity::init`.
  A durable update is a single cross-process transaction and must never run on
  a UI thread. Not registering is not silent — the first write logs a warning
  naming `init_scheduler`, `scheduler_is_registered()` answers a doctor
  command, and core falls back to a writer thread of its own so the organism
  still remembers — but the fallback is one thread with a bounded queue and no
  knowledge of the app's other writes, so it is a floor rather than a lane.
  `flush_pending(timeout)` belongs in the shutdown drain ahead of the app's own
  persistence shutdown; its deadline bounds the caller, not the transaction.
- Provider-neutral AI requests, bounded conversations, secret redaction, and
  the review-first `jagent` session surface. Request construction reports any
  omitted history, while system instructions are rejected rather than sampled
  when they or the complete omission notice exceed jagent's 64 KiB limit.
  Validated loopback HTTP requests force curl to bypass environment proxies,
  keeping their clear-text credential hop local while preserving normal proxy
  behavior for HTTPS.

## Security and reliability invariants

1. Untrusted terminal data is size-bounded before allocation or parsing.
2. Persistence uses regular-file/owner/link checks, safe parent directories,
   atomic durable replacement, and bounded reads. Lock-using stores add
   time-bounded cross-process locks whose namespace cannot be split by
   replacing a sidecar path. FIFOs, devices, unsafe writable parents, and
   symlink targets fail closed. On Linux, Android, and macOS, Agent restore
   claims use one atomic no-replace rename: a process crash leaves either the
   public snapshot or one `.claimed-*` evidence file, never a transient extra
   hard link. Other platforms fail closed when they cannot provide that
   primitive. `try_claim_session_file` exposes every non-missing claim failure
   as `io::Error`; the legacy best-effort wrapper logs and collapses it to a
   vacant outcome without falling back to a separate read, and is deprecated
   together with the racy legacy read/remove pair. Agent claims sync
   the parent after retiring the public name before a live session is exposed,
   then unlink and sync the consumed private claim. A failed retirement barrier
   exposes no session and preserves the evidence; a failed post-unlink cleanup
   sync is logged but the already non-replayable live session remains usable.
3. The ASCII organism's memory file follows the same lock-using store rules as
   the execution journal: a private, user-owned parent directory, a fixed
   `<memory>.lock` sidecar, and time-bounded `flock`s on both taken in one
   order, then a bounded reread and private atomic replacement inside them. The
   directory and its sidecar form one trust boundary, so neither can be
   replaced to split the namespace. Events are released only after a
   transaction succeeds, so a failed or dropped write delays an update rather
   than losing it; once a path's queue holds its 256-event maximum, admission
   rejects with `WouldBlock` and the in-memory view is deliberately held back
   so it cannot diverge from disk.
4. Restored argv boundaries are preserved. Legacy joined command strings may
   be read for migration but are never replayed.
5. Model output is only a proposal. Every command requires explicit user
   review; the historical read-only auto-approval hook always fails closed.
6. UI event loops must adapt these synchronous primitives without moving GUI
   objects across threads.
7. Executable cache materialization (including the embedded jsh installer) is
   private, bounded, no-follow, and content-verified before launch; custom
   themes and provider-key files use the same hostile-filesystem policy.
8. Atomic replacement on Unix is relative to one validated directory
   descriptor (`openat`/`renameat`/`unlinkat`), so replacing a pathname during
   a save cannot redirect the commit. Shared writable parents require sticky
   semantics and temporary names include OS entropy.
9. Git metadata subprocess output, queues, cache entries, waiters, branch
   labels, and lifetime are bounded. Short-lived helpers are supervised under
   one absolute deadline; on Unix their root remains waitable until the fresh
   process group is cleared and is then synchronously reaped, including when a
   descendant retains a pipe. This ownership requires a waitable `SIGCHLD`
   disposition and no external waiter for that exact child; the final kernel
   wait after a logical deadline may exceed wall time for uninterruptible I/O.
   Repository-configured hooks/FSMonitor programs are disabled. Notebook
   splitting rejects oversized input and fence storms, and Pango rendering
   exposes control/invisible/bidirectional formatting instead of displaying a
   reordered review surface.
10. Review-only text uses one shared visual-spoof predicate (non-ASCII spacing,
    bidi, zero-width, and default-ignorable formatting). Prompt insertion,
    restored argv, history metadata, shell-quoted paths, and theme names fail
    closed; clipboard paste reports the risk for explicit confirmation.
11. AI curl and jsh update-check pipes are drained nonblocking with byte/time
    caps and process-group cleanup. Slow consumers apply kernel backpressure;
    a descendant retaining stdout cannot pin a reader thread. Successful
    non-streaming AI bodies are decoded only after jagent's 1 MiB gate, while
    non-2xx JSON is parsed only within the 2 KiB diagnostic budget.
12. A completed block is successful only when a frontend-resolved, non-blank
    command has an explicitly reported exit code of zero. An absent resolved
    command is background output, and a missing exit code remains unknown;
    neither is rewritten into a synthetic success or failure. Resolution must
    apply protocol/screen fallback before classification, while command review
    and display sanitization remain separate boundaries.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo test --doc
cargo doc --no-deps
```

When changing a shared API, validate all four terminal consumers and jsh in a
coordinated change. Git dependencies should be pinned to an audited revision;
do not make a consumer depend on a new local API until that revision is
available to a clean checkout.

## License

MIT
