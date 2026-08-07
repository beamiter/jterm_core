# jterm_core

`jterm_core` is the UI-independent foundation shared by anvil, ember,
frost, and forge. It contains terminal protocol, process, persistence,
AI/Agent, and policy code that must behave identically across GTK, egui, and
iced frontends.

The crate deliberately has no GUI-toolkit dependency. Frontends own widgets,
rendering, and event-loop integration; this crate owns byte-level protocols,
bounds, validation, and operating-system primitives.

## Shared surfaces

- OSC/CSI/DCS/APC parsing, Kitty graphics framing, character widths, themes,
  the family keybinding grammar, and the four-way completed-block outcome
  contract (background, success, failure, or unknown status).
- PTY input guarding, review-only command insertion, child environments,
  process-group lifecycle management, and restorable-command quoting.
- Private atomic snapshots, command history, jsh execution journals, pane
  layouts, Git metadata, notifications, and host/Flatpak command routing.
- Provider-neutral AI requests, bounded conversations, secret redaction, and
  the review-first `jagent` session surface.

## Security and reliability invariants

1. Untrusted terminal data is size-bounded before allocation or parsing.
2. Persistence uses regular-file/owner/link checks, safe parent directories,
   atomic durable replacement, and bounded reads. Lock-using stores add
   time-bounded cross-process locks whose namespace cannot be split by
   replacing a sidecar path. FIFOs, devices, unsafe writable parents, and
   symlink targets fail closed.
3. Restored argv boundaries are preserved. Legacy joined command strings may
   be read for migration but are never replayed.
4. Model output is only a proposal. Every command requires explicit user
   review; the historical read-only auto-approval hook always fails closed.
5. UI event loops must adapt these synchronous primitives without moving GUI
   objects across threads.
6. Executable cache materialization (including the embedded jsh installer) is
   private, bounded, no-follow, and content-verified before launch; custom
   themes and provider-key files use the same hostile-filesystem policy.
7. Atomic replacement on Unix is relative to one validated directory
   descriptor (`openat`/`renameat`/`unlinkat`), so replacing a pathname during
   a save cannot redirect the commit. Shared writable parents require sticky
   semantics and temporary names include OS entropy.
8. Git metadata subprocess output, queues, cache entries, waiters, branch
   labels, and lifetime are bounded; process-group cleanup handles descendants
   that retain stdout, and repository-configured hooks/FSMonitor programs are
   disabled. Notebook splitting rejects oversized input and fence storms, and
   Pango rendering exposes control/invisible/bidirectional formatting instead
   of displaying a reordered review surface.
9. Review-only text uses one shared visual-spoof predicate (non-ASCII spacing,
   bidi, zero-width, and default-ignorable formatting). Prompt insertion,
   restored argv, history metadata, shell-quoted paths, and theme names fail
   closed; clipboard paste reports the risk for explicit confirmation.
10. AI curl and jsh update-check pipes are drained nonblocking with byte/time
    caps and process-group cleanup. Slow consumers apply kernel backpressure;
    a descendant retaining stdout cannot pin a reader thread.
11. A completed block is successful only when a frontend-resolved, non-blank
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
cargo doc --no-deps
```

When changing a shared API, validate all four terminal consumers and jsh in a
coordinated change. Git dependencies should be pinned to an audited revision;
do not make a consumer depend on a new local API until that revision is
available to a clean checkout.

## License

MIT
