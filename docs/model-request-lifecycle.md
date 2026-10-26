# Request-bound model callbacks

The shared Agent adapter pins jagent `f269c3c6afafcf9b6d60b3cccd741649fdb74576`.
Capture `model_request_token()` once at dispatch. Carry that token through
success, provider errors, transport failure and worker disconnection. Use the
`*_for` methods; never look up the current token from an asynchronous callback.
`ModelRequestError::StaleRequest` is a discarded event, not a failure of the
current request. Do not retry it or change the current worker's UI state.

## Audited consumer paths

| Consumer | Dispatch and completion | Cancellation, retry and restore |
| --- | --- | --- |
| jsh | The synchronous `builtin_agent` loop captures before `request_model`; completed protocol-aware responses and failures use the captured token. Streaming decoding remains inside the existing bounded transport. | Interrupt checks precede ingestion. Only a matching failure may offer retry. Each retry obtains a new token. |
| Anvil | `begin_model_request` binds a jagent token to the existing epoch/generation ticket. `apply_llm_reply` checks that ticket and uses checked ingestion, including oversized and invalid replies. | Stop retires the model attempt before retry; session replacement and cancellation invalidate the old token. UI and command execution tickets remain separate. |
| Forge | `request_model` captures token and session epoch before worker launch. Its GTK polling closure checks both before touching busy state or cancellation slots. Success, error and disconnected-worker paths use checked APIs. | Stopped transport reports failure only for its original attempt. Old polling closures terminate without modifying a replacement session. |
| Ember | Each owned result receiver retains its dispatch token and epoch. Success, failure and disconnection use that token. Worker spawn failure consumes the dispatch token. | Dropping a receiver on reset also clears its token. Retry and restored sessions get fresh jagent identities. |
| Frost | The existing epoch/generation maps to a token held in `InFlightModelRequest`. Stream preview callbacks validate that active attempt; final text and errors use checked ingestion. | Oversize stream cancellation fails only its owning attempt. Stale final/delta events cannot clear a new preview, worker handle or loading flag. |

Synchronous configuration/consent failures, explicit Stop actions and local
restore validation failures are not transport callbacks. Existing unguarded
compatibility entry points remain for those local transitions and fixtures.
No snapshot fields, persisted task formats, prompt bytes, command approval
rules or execution capabilities were changed. Tokens are process-local and
are never serialized. The existing epoch/proposal/execution correlation
continues to protect command review and execution independently.

The four applications use a sibling path dependency, with reproducible
`packaging/jterm-core-src.tar.gz` snapshots for standalone installs. Updating
only a git revision in a manifest would not deliver this adapter to them.
Refresh the snapshots and their checksums, lockfiles, Flatpak git source refs,
source allowlists and Nix fixed-output hashes together.

## Regression coverage

`tests/model_request_lifecycle.rs` exercises delayed success/failure,
duplicate callbacks, cross-session and restored callbacks, cancellation,
concurrent failure/retry competition, and the unchanged review boundary.
The Ember/Frost app validators also reject stale tokens before checkpoint or
proposal validation can mutate a replacement session. Frost tests late stream
fragments and failure after retry while preserving the active worker preview.
Anvil's existing stop/retry/duplicate callback tests exercise its migrated
request ticket adapter.
