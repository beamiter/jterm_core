# Cross-adapter session parity corpus

The eight synthetic PTY traces in `corpus.json` have explicit semantic
checkpoints for ordinary shell commands. They do not arm Agent executions or
change the adapters' separate Agent trust and lost-execution cleanup policy. They are not recordings of a real shell and are not a substitute
for each frontend's real Bash PTY or native display tests.

Enable `jterm_core/session-parity-tests` in test/dev dependencies to consume
`session_parity::fixtures`, `CHUNKINGS`, and `chunks`. Feed the bytes through
an adapter's existing production parser and lifecycle owner, then inspect its
actual state. Never implement a second lifecycle reducer in the fixture driver.

- `pty` contains output bytes, including shell-generated OSC 133 and terminal
  controls. It is UTF-8 source data; partitioning deliberately cuts within
  UTF-8, CSI, and OSC sequences.
- `surface` is the native Chat surface after the whole semantic step, not an
  assertion about transient states halfway through a split control packet.
- `completed` is cumulative, ordered, accepted command completion data.
  `exit_code: null` means no exit status was reported, never success.
- `draft_text`, when present, is read from actual native cells and checked with
  the native cursor width; only grid-padding whitespace may be normalized.
- `running_id` and completion `id` are shell execution correlation identities.
  Frontend numeric block IDs need only remain stable within that frontend.
- The command text on `C` is percent-encoded `cmdline_url`, with a full Start
  identity envelope. Frontends must not replace it with reconstructed display
  text. In the mismatch/dedup case, each `D` is tagged with `id`; that
  case does not test untagged legacy completion.
- Interrupt output (`^C`) alone is not a completion. This trace does not send
  Ctrl-C or test PTY write admission; dedicated input/PTY tests cover that.

Replay each case into a fresh adapter for Whole, Bytewise, and Staggered
partitions. At selected checkpoints, existing real projection seams should
also check that changing presentation preserves canonical records, raw cells,
cursor, dimensions, and local identities. Renderer caches may change. A
recording GTK backend cannot certify native VTE geometry or reparenting.

The shared core tests validate corpus shape, lossless partitioning, and the
production stream parser's event invariance. The core parser intentionally
emits protocol events, including stray D marks; correlation and dedup belong
to the existing frontend lifecycle owners. Adapter tests are the semantic gate.
