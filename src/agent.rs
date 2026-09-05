//! Review-first agent core, shared with jsh through the `jagent` crate.
//!
//! The pure session state machine, action parser, safety heuristics, and
//! snapshot serialization live in `jagent` (sans-IO). This module re-exports
//! that surface under the historical `jterm_core::agent` paths and adds the
//! filesystem persistence helpers the jterm apps use for
//! `<config-dir>/<app>/agent_session.json`.

pub use jagent::agent::{
    prepare_agent_request, AgentRequestReport, AgentRequestSpec, PreparedAgentRequest,
};
pub use jagent::capabilities::{
    agent_capabilities, agent_capabilities_for_peer, agent_capabilities_v2, AgentCapabilities,
    AgentDelivery, CapabilityError, AGENT_CAPABILITIES_V1_WIRE, AGENT_CAPABILITIES_V2_WIRE,
    AGENT_CAPABILITIES_VERSION, MAX_AGENT_CAPABILITIES_WIRE_BYTES,
};
pub use jagent::provider::{Message as AgentMessage, Provider as AgentProvider, Role as AgentRole};
pub use jagent::response::{AgentResponse, AgentStream};
// jagent owns the shared command-text contract: the ceiling, the typed reason,
// and the check itself. Re-exported rather than reimplemented so the four apps
// and this crate enforce one rule — a local copy of the ceiling or of the
// predicate stops widening the day jagent's does, and nothing fails until a
// model reply aims at the difference.
pub use jagent::safety::{
    is_dangerous, validate_command_text, CommandTextError, MAX_COMMAND_BYTES,
};
pub use jagent::session::{
    parse_action, sample_observation, AgentSessionSnapshot, AgentSnapshotError, AgentState,
    ApprovedCommand, CancellationToken, CommandExecutionFailure, CommandExecutionOutcome,
    ModelOutcome, ParseError, ParsedAction, ProposalId, ProposalStatus, SessionError, Turn,
    MAX_AGENT_SNAPSHOT_JSON_BYTES,
};
pub use jagent::tools::AgentProtocol;
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_SESSION_TURNS: u32 = 1_000;
const MAX_STORED_TRANSCRIPT_ENTRIES: usize = 128;
const MAX_STORED_TRANSCRIPT_BYTES: usize = 128 * 1024;
const MAX_OBSERVATION_BYTES: usize = 4 * 1024;
const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_THOUGHT_BYTES: usize = 4 * 1024;
static NEXT_SESSION_EPOCH: AtomicU64 = AtomicU64::new(1);

/// Process-local identity for one task generation of an Agent session.
///
/// The exact-pinned jagent resets proposal ids when `start_new_task` is used.
/// A UI callback must therefore capture this epoch together with its
/// [`ProposalId`] and check both before approving, rejecting, editing, or
/// attaching an observation. The epoch also changes across a replacement
/// [`AgentSession`], not only an in-place task reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AgentSessionEpoch(u64);

impl AgentSessionEpoch {
    pub fn get(self) -> u64 {
        self.0
    }
}

fn next_session_epoch() -> AgentSessionEpoch {
    // Exhausting a 64-bit process-local counter would require creating more
    // sessions than the process can execute instructions. Keep zero reserved
    // and make the wrap behavior explicit rather than silently returning it.
    let epoch = NEXT_SESSION_EPOCH.fetch_add(1, Ordering::Relaxed);
    assert!(
        epoch != 0 && epoch != u64::MAX,
        "Agent session epoch exhausted"
    );
    AgentSessionEpoch(epoch)
}

/// Hardened compatibility wrapper around the exact-pinned jagent session.
///
/// jagent validates its own snapshot invariants; this wrapper preserves the
/// family contract independently and adds a process-local task epoch. Keeping
/// the inner type private ensures every restore reached through
/// `jterm_core::agent` first audits jagent's bounded, immutable snapshot view.
#[derive(Debug)]
pub struct AgentSession {
    inner: jagent::session::AgentSession,
    epoch: AgentSessionEpoch,
}

impl AgentSession {
    pub fn new(max_turns: u32) -> Self {
        Self {
            inner: jagent::session::AgentSession::new(max_turns.clamp(1, MAX_SESSION_TURNS)),
            epoch: next_session_epoch(),
        }
    }

    /// Restore only snapshots whose proposal ids, statuses, and state form a
    /// state machine that public live transitions could have produced.
    pub fn restore(snapshot: AgentSessionSnapshot) -> Result<Self, AgentSnapshotError> {
        validate_snapshot(&snapshot)?;
        Ok(Self {
            inner: jagent::session::AgentSession::restore(snapshot)?,
            epoch: next_session_epoch(),
        })
    }

    pub fn epoch(&self) -> AgentSessionEpoch {
        self.epoch
    }

    pub fn is_current_epoch(&self, epoch: AgentSessionEpoch) -> bool {
        self.epoch == epoch
    }

    pub fn transcript(&self) -> &[Turn] {
        self.inner.transcript()
    }

    pub fn state(&self) -> AgentState {
        self.inner.state()
    }

    pub fn turns_used(&self) -> u32 {
        self.inner.turns_used()
    }

    pub fn max_turns(&self) -> u32 {
        self.inner.max_turns()
    }

    pub fn can_continue_after_completion(&self) -> bool {
        self.inner.can_continue_after_completion()
    }

    pub fn continue_after_completion(&mut self) -> Result<(), SessionError> {
        self.inner.continue_after_completion()
    }

    pub fn start_new_task(&mut self) -> Result<(), SessionError> {
        self.inner.start_new_task()?;
        self.epoch = next_session_epoch();
        Ok(())
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.inner.cancellation_token()
    }

    pub fn submit_user(&mut self, message: impl Into<String>) -> Result<(), SessionError> {
        self.inner.submit_user(message)
    }

    pub fn accept_model_reply(&mut self, raw: &str) -> Result<ModelOutcome, SessionError> {
        let outcome = self.inner.accept_model_reply(raw)?;
        self.validate_live_outcome(outcome)
    }

    pub fn accept_model_tool_reply(
        &mut self,
        reply: &jagent::tools::ToolResponse,
    ) -> Result<ModelOutcome, SessionError> {
        let outcome = self.inner.accept_model_tool_reply(reply)?;
        self.validate_live_outcome(outcome)
    }

    /// Ingest the protocol-aware response produced by a
    /// [`PreparedAgentRequest`].
    ///
    /// This is the preferred 0.7 path: provider, text/native-tools protocol,
    /// generation-limit handling, and response decoding stay bound to the
    /// request that created the response. Historical text/tool entry points
    /// remain available for compatible frontends.
    pub fn accept_agent_response(
        &mut self,
        response: &AgentResponse,
    ) -> Result<ModelOutcome, SessionError> {
        let outcome = self.inner.accept_agent_response(response)?;
        self.validate_live_outcome(outcome)
    }

    pub fn model_failed(&mut self, message: impl Into<String>) -> Result<(), SessionError> {
        self.inner.model_failed(message)
    }

    pub fn retry_model(&mut self) -> Result<(), SessionError> {
        self.inner.retry_model()
    }

    pub fn can_retry_model(&self) -> bool {
        self.inner.can_retry_model()
    }

    pub fn approve(&mut self, id: ProposalId) -> Result<ApprovedCommand, SessionError> {
        self.inner.approve(id)
    }

    pub fn edit_and_approve(
        &mut self,
        id: ProposalId,
        edited_command: impl Into<String>,
    ) -> Result<ApprovedCommand, SessionError> {
        let edited_command = edited_command.into();
        validate_agent_command(&edited_command).map_err(invalid_command_error)?;
        self.inner.edit_and_approve(id, edited_command)
    }

    pub fn reject(&mut self, id: ProposalId) -> Result<(), SessionError> {
        self.inner.reject(id)
    }

    pub fn reject_with_feedback(
        &mut self,
        id: ProposalId,
        feedback: impl Into<String>,
    ) -> Result<(), SessionError> {
        self.inner.reject_with_feedback(id, feedback)
    }

    pub fn edit_for_manual_review(
        &mut self,
        id: ProposalId,
        edited_command: impl Into<String>,
    ) -> Result<String, SessionError> {
        let edited_command = edited_command.into();
        validate_agent_command(&edited_command).map_err(invalid_command_error)?;
        self.inner.edit_for_manual_review(id, edited_command)
    }

    pub fn observe(
        &mut self,
        id: ProposalId,
        exit_code: i32,
        output: &str,
    ) -> Result<(), SessionError> {
        self.inner.observe(id, exit_code, output)
    }

    /// Ingest the executor's typed result without synthesizing a status for
    /// start, timeout, or cancellation failures.
    pub fn observe_execution(
        &mut self,
        id: ProposalId,
        outcome: CommandExecutionOutcome,
    ) -> Result<(), SessionError> {
        self.inner.observe_execution(id, outcome)
    }

    pub fn observe_execution_failure(
        &mut self,
        id: ProposalId,
        failure: CommandExecutionFailure,
        detail: &str,
    ) -> Result<(), SessionError> {
        self.inner.observe_execution_failure(id, failure, detail)
    }

    pub fn cancel(&mut self) {
        self.inner.cancel();
    }

    pub fn build_user_prompt(&self) -> String {
        self.inner.build_user_prompt()
    }

    pub fn build_user_prompt_with(&self, protocol: AgentProtocol) -> String {
        self.inner.build_user_prompt_with(protocol)
    }

    pub fn snapshot(&self) -> Option<AgentSessionSnapshot> {
        self.inner.snapshot()
    }

    fn validate_live_outcome(
        &mut self,
        outcome: ModelOutcome,
    ) -> Result<ModelOutcome, SessionError> {
        let ModelOutcome::Proposal { id, command, .. } = &outcome else {
            return Ok(outcome);
        };
        let Err(reason) = validate_agent_command(command) else {
            return Ok(outcome);
        };

        // The pinned state machine already created a pending transcript turn.
        // Reject it immediately so no approval API can observe an unsafe live
        // proposal even if a frontend forgets its own review-input check.
        self.inner.reject(*id)?;
        Err(invalid_command_error(reason))
    }
}

/// The family's own ceilings on a persisted snapshot, applied before jagent's
/// restore audits the same document.
///
/// This used to carry a ~135-line near-copy of jagent's
/// `validate_snapshot_lifecycle`: the same state/final-turn binding, the same
/// turn-counter arithmetic, the same approved-proposal outcome rule, and a
/// duplicate of its documented-diagnostic matcher. A copy of a rule that runs
/// FIRST is the worst place for one to rot, because the weaker of the two is
/// then the one that always answers — and it had already rotted. jagent has
/// since gained transcript-shape adjacency rules the copy never had (a model
/// action must follow a model-request boundary, a thought must sit against its
/// own action, a protocol error needs an outstanding operation, an observation
/// must immediately follow its approved proposal), and two of the copy's reason
/// strings had drifted from the wording the user actually reads.
///
/// So the lifecycle is jagent's now. What stays here is what jagent's restore
/// does not decide the same way:
///
/// * the transcript's ENCODED size. jagent bounds the prompt reconstruction of
///   a transcript at 128 KiB and one whole snapshot document at
///   [`MAX_AGENT_SNAPSHOT_JSON_BYTES`] (256 KiB); neither is a bound on the
///   JSON the family actually writes to `agent_session.json` and reads back.
/// * a stale next proposal id. jagent REPAIRS one, taking the maximum of the
///   stored value and `highest + 1`. Proposal ids are the binding between an
///   approval card and the command handed back on approval, so a document whose
///   counter disagrees with its own transcript is evidence of editing, and this
///   family refuses it rather than continuing from a silently corrected value.
/// * the per-turn text budgets. jagent enforces the same numbers today, but
///   these are the family's own ceilings — the apps size their storage and
///   their panels against these constants — so they are checked here rather
///   than assumed to stay wherever jagent's happen to sit.
fn validate_snapshot(snapshot: &AgentSessionSnapshot) -> Result<(), AgentSnapshotError> {
    // jagent decoded this immutable view through allocation-aware seeds. Audit
    // it directly: re-serializing into an ordinary `Vec<Turn>` decoder would
    // create a second, weaker wire path around that boundary.
    let transcript = snapshot.transcript();
    if transcript.len() > MAX_STORED_TRANSCRIPT_ENTRIES {
        return Err(AgentSnapshotError::Invalid(
            "transcript exceeds its entry limit",
        ));
    }
    let transcript_bytes = serde_json::to_vec(transcript)
        .map_err(|error| AgentSnapshotError::Encode(error.to_string()))?
        .len();
    if transcript_bytes > MAX_STORED_TRANSCRIPT_BYTES {
        return Err(AgentSnapshotError::Invalid(
            "transcript exceeds its byte limit",
        ));
    }

    // No wildcard arm: a turn variant jagent adds must be a compile error
    // here, not a shape that silently carries unbudgeted text.
    let mut highest_proposal_id = 0_u64;
    for turn in transcript {
        match turn {
            Turn::User(message) | Turn::AssistantSay(message) => {
                validate_snapshot_text(message, MAX_MESSAGE_BYTES, true)?;
            }
            Turn::AssistantThought(thought) => {
                validate_snapshot_text(thought, MAX_THOUGHT_BYTES, true)?;
            }
            Turn::AssistantProposed { id, .. } => {
                highest_proposal_id = highest_proposal_id.max(id.get());
            }
            Turn::Observation { output_sample, .. } => {
                if output_sample.len() > MAX_OBSERVATION_BYTES {
                    return Err(AgentSnapshotError::Invalid(
                        "observation violates its safety bounds",
                    ));
                }
            }
            Turn::ProtocolError(message) => {
                validate_snapshot_text(message, MAX_MESSAGE_BYTES, false)?;
            }
        }
    }

    // Ordering, duplication and every id/status/state binding are jagent's;
    // only the counter's own consistency with the transcript is decided here,
    // because that is the one jagent repairs instead of refusing.
    if snapshot.next_proposal_id() == 0
        || snapshot.next_proposal_id() == u64::MAX
        || snapshot.next_proposal_id() <= highest_proposal_id
    {
        return Err(AgentSnapshotError::Invalid(
            "next proposal id is stale or exhausted",
        ));
    }
    Ok(())
}

fn validate_snapshot_text(
    value: &str,
    max_bytes: usize,
    require_nonempty: bool,
) -> Result<(), AgentSnapshotError> {
    if value.len() > max_bytes || (require_nonempty && value.trim().is_empty()) {
        return Err(AgentSnapshotError::Invalid(
            "transcript text violates its safety bounds",
        ));
    }
    Ok(())
}

/// The family's `&'static str` spelling of jagent's command-text contract.
///
/// The rule itself is [`validate_command_text`]; this is only the mapping from
/// its typed reason onto the strings this module has always returned, which
/// reach the user through [`ParseError::InvalidCommand`]. The reimplementation
/// this replaces had already drifted in shape from jagent's (it checked empty
/// before size, and knew nothing of the line-break case), and a fork of the
/// invisible-character table is exactly the failure the shared crate exists to
/// prevent.
fn validate_agent_command(command: &str) -> Result<(), &'static str> {
    validate_command_text(command)
        .map(|_| ())
        .map_err(|error| match error {
            CommandTextError::Empty => "command must not be empty",
            // The literal byte count is pinned to jagent's ceiling by a test,
            // so a change there is a red suite rather than a message that
            // quotes a limit the code no longer enforces.
            CommandTextError::TooLarge => "command exceeds the 16384-byte safety limit",
            // jagent separates a line break from other controls; this module
            // never has, and a line break IS a control character, so keep the
            // spelling the family already ships rather than introduce a new
            // user-visible string for text that was refused before.
            CommandTextError::LineBreak | CommandTextError::ControlCharacter => {
                "command contains a control character"
            }
            CommandTextError::VisualSpoof => {
                "command contains invisible or bidirectional formatting"
            }
            // `CommandTextError` is `#[non_exhaustive]`: a reason jagent adds
            // must still refuse the command here, with a reason that does not
            // claim to know more than it does.
            _ => "command violates the shared command-text contract",
        })
}

fn invalid_command_error(reason: &'static str) -> SessionError {
    SessionError::Protocol(ParseError::InvalidCommand(reason.to_string()))
}

/// Keep the historical family API fail-closed.
///
/// A string-only classifier cannot prove what a terminal's child shell will
/// actually execute: aliases and functions can replace an apparently harmless
/// program, repository configuration can make read-looking tools launch
/// helpers, and successful reads can expose sensitive data to the next model
/// turn. Frontends must therefore keep every proposal behind explicit review
/// until an integration-specific execution policy can validate the resolved
/// command and its data access.
pub fn is_auto_approvable(_command: &str) -> bool {
    false
}

/// Persist a snapshot to `path` with private (0600) permissions, creating
/// parent directories and replacing atomically via a sibling temp file.
pub fn write_snapshot_file(
    path: &std::path::Path,
    snapshot: &AgentSessionSnapshot,
) -> Result<(), AgentSnapshotError> {
    let encoded = snapshot.to_json()?;
    crate::snapshot_file::write_atomic_private(path, encoded.as_bytes())
        .map_err(|error| AgentSnapshotError::Encode(format!("write {}: {error}", path.display())))
}

/// Best-effort bounded read of a snapshot file. Any failure (missing file,
/// oversize, parse error) yields None — a broken snapshot must never block
/// opening a fresh session.
#[deprecated(note = "use try_claim_session_file for an atomic, typed, durability-owning restore")]
pub fn read_snapshot_file(path: &std::path::Path) -> Option<AgentSessionSnapshot> {
    let encoded =
        crate::snapshot_file::read_bounded(path, MAX_AGENT_SNAPSHOT_JSON_BYTES as u64).ok()?;
    AgentSessionSnapshot::from_json(&encoded).ok()
}

/// Remove a persisted snapshot; missing files are fine.
#[deprecated(
    note = "use try_claim_session_file for restore/consume, or an application-owned durable deletion transaction for explicit discard"
)]
pub fn remove_snapshot_file(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

/// Outcome of [`try_claim_session_file`] and [`claim_session_file`].
#[derive(Debug)]
pub enum SessionClaim {
    /// Nothing to restore: no snapshot existed or another opener claimed it.
    ///
    /// The compatibility [`claim_session_file`] wrapper also returns this
    /// outcome after logging a non-missing claim error. Call
    /// [`try_claim_session_file`] when the distinction matters.
    Vacant,
    /// This caller won the claim and the session was restored. The persisted
    /// snapshot has been consumed.
    Restored(AgentSession),
    /// This caller won the claim, but the evidence could not become a session.
    /// It has been moved aside at `path` rather than deleted, so a corrupt or
    /// hostile snapshot stays available for inspection.
    Quarantined {
        path: std::path::PathBuf,
        error: AgentSnapshotError,
    },
}

/// Atomically claim a persisted snapshot and consume it into a live session.
///
/// Restoring as a `read_snapshot_file` followed by a separate
/// `remove_snapshot_file` is racy in two ways: two windows opening at once can
/// both read the same snapshot and both resume it, and a crash between the two
/// calls either loses the session or replays it. This primitive closes both:
/// the snapshot is moved to a private name first, so exactly one caller ever
/// observes it, the retired public name is durably synced before any session
/// is exposed, and the claim is only deleted once a session exists. Its cleanup
/// sync is attempted before return; failure can leave only an ignored private
/// orphan after a crash, never replay the durably retired public snapshot.
///
/// A missing public name, including losing a race to another opener, returns
/// [`SessionClaim::Vacant`]. Any other failure to acquire the claim is returned
/// unchanged as an [`std::io::Error`]; the function never falls back to a
/// separate read. Once the claim succeeds, invalid evidence returns
/// [`SessionClaim::Quarantined`] so its private claim path is not lost.
pub fn try_claim_session_file(path: &std::path::Path) -> std::io::Result<SessionClaim> {
    try_claim_session_file_with(path, crate::snapshot_file::claim_exclusive)
}

fn try_claim_session_file_with(
    path: &std::path::Path,
    claim: impl FnOnce(&std::path::Path) -> std::io::Result<std::path::PathBuf>,
) -> std::io::Result<SessionClaim> {
    try_claim_session_file_with_sync(path, claim, crate::snapshot_file::sync_parent_directory)
}

fn try_claim_session_file_with_sync(
    path: &std::path::Path,
    claim: impl FnOnce(&std::path::Path) -> std::io::Result<std::path::PathBuf>,
    mut sync_parent: impl FnMut(&std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<SessionClaim> {
    let claimed = match claim(path) {
        Ok(claimed) => claimed,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Missing file or a lost race: do not fall back to reading the
            // public name after another opener may have retired it.
            return Ok(SessionClaim::Vacant);
        }
        Err(error) => return Err(error),
    };
    // Persist retirement of the public name before decoding can expose a live
    // session. Without this barrier a power loss may roll the rename back and
    // let a later process replay the same approval snapshot.
    sync_parent(&claimed)?;
    // Transcripts can contain private data, so a claimed snapshot with any
    // group/other permission bits is treated as tampering, not merely read.
    let restored =
        crate::snapshot_file::read_bounded_private(&claimed, MAX_AGENT_SNAPSHOT_JSON_BYTES as u64)
            .map_err(|error| {
                AgentSnapshotError::Decode(format!("read {}: {error}", claimed.display()))
            })
            .and_then(|encoded| AgentSessionSnapshot::from_json(&encoded))
            .and_then(AgentSession::restore);
    Ok(match restored {
        Ok(session) => {
            // Do not report a consumed session while its private claim could
            // still survive this process. A failure is surfaced through the
            // typed I/O result and the evidence remains at its claim path.
            std::fs::remove_file(&claimed)?;
            // The first barrier already made retirement of the public name
            // durable, so a later process can never replay this session. If
            // syncing the cleanup fails, the worst crash outcome is an orphan
            // under the private `.claimed-*` name, which loaders deliberately
            // ignore. Keep the live session available and surface that
            // maintenance issue through the log instead of losing both it and
            // the now-unlinked evidence.
            if let Err(error) = sync_parent(&claimed) {
                log::warn!(
                    "agent: consumed snapshot cleanup for {} was not durable: {error}",
                    claimed.display()
                );
            }
            SessionClaim::Restored(session)
        }
        Err(error) => SessionClaim::Quarantined {
            path: claimed,
            error,
        },
    })
}

fn collapse_claim_result(
    path: &std::path::Path,
    result: std::io::Result<SessionClaim>,
) -> SessionClaim {
    match result {
        Ok(claim) => claim,
        Err(error) => {
            log::warn!(
                "agent: could not atomically claim saved session {}: {error}",
                path.display()
            );
            SessionClaim::Vacant
        }
    }
}

/// Compatibility wrapper around [`try_claim_session_file`].
///
/// Non-missing claim failures are logged and collapsed to
/// [`SessionClaim::Vacant`], preserving the historical best-effort behavior.
/// New integrations should use the typed entry point when an unavailable safe
/// claim primitive or an I/O policy failure must be visible.
#[deprecated(note = "use try_claim_session_file so non-missing claim failures remain typed")]
pub fn claim_session_file(path: &std::path::Path) -> SessionClaim {
    collapse_claim_result(path, try_claim_session_file(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compatibility regressions deliberately exercise these deprecated
    // wrappers through three narrowly allowed adapters. Production code and
    // every non-compatibility test keep deprecation warnings visible.
    #[allow(deprecated)]
    fn legacy_read_snapshot_file(path: &std::path::Path) -> Option<AgentSessionSnapshot> {
        read_snapshot_file(path)
    }

    #[allow(deprecated)]
    fn legacy_remove_snapshot_file(path: &std::path::Path) {
        remove_snapshot_file(path);
    }

    #[allow(deprecated)]
    fn legacy_claim_session_file(path: &std::path::Path) -> SessionClaim {
        claim_session_file(path)
    }

    fn pending_snapshot() -> AgentSessionSnapshot {
        let mut session = AgentSession::new(10);
        session.submit_user("list files").unwrap();
        session
            .accept_model_reply(r#"{"action":"run","command":"printf reviewed"}"#)
            .unwrap();
        session.snapshot().unwrap()
    }

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "jterm-core-agent-{label}-{}-{id}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn typed_claim_errors_distinguish_missing_from_hard_failures_without_fallback() {
        let dir = TestDir::new("claim-injected-errors");
        let path = dir.0.join("agent_session.json");
        std::fs::write(&path, b"public evidence stays put").unwrap();

        for (kind, vacant) in [
            (std::io::ErrorKind::NotFound, true),
            (std::io::ErrorKind::Unsupported, false),
            (std::io::ErrorKind::PermissionDenied, false),
        ] {
            let calls = std::cell::Cell::new(0);
            let result = try_claim_session_file_with(&path, |_| {
                calls.set(calls.get() + 1);
                Err(std::io::Error::new(kind, "injected claim failure"))
            });
            assert_eq!(calls.get(), 1, "claim backend must run exactly once");
            if vacant {
                assert!(matches!(result.unwrap(), SessionClaim::Vacant));
            } else {
                assert_eq!(result.unwrap_err().kind(), kind);
            }
            assert_eq!(
                std::fs::read(&path).unwrap(),
                b"public evidence stays put",
                "a claim error must not fall back to reading or retiring the public name"
            );
        }
    }

    #[test]
    fn compatibility_claim_collapses_a_typed_error_to_vacant() {
        let dir = TestDir::new("claim-legacy-collapse");
        let path = dir.0.join("agent_session.json");
        std::fs::write(&path, b"public evidence stays put").unwrap();
        let result = try_claim_session_file_with(&path, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "injected unavailable primitive",
            ))
        });

        assert!(matches!(
            collapse_claim_result(&path, result),
            SessionClaim::Vacant
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"public evidence stays put");
    }

    #[test]
    fn injected_claim_restores_valid_evidence_and_quarantines_invalid_evidence() {
        let dir = TestDir::new("claim-injected-evidence");
        let public = dir.0.join("agent_session.json");
        let valid_claim = dir.0.join("agent_session.valid-claim.json");
        write_snapshot_file(&valid_claim, &pending_snapshot()).unwrap();

        let SessionClaim::Restored(session) = try_claim_session_file_with(&public, |path| {
            assert_eq!(path, public);
            Ok(valid_claim.clone())
        })
        .unwrap() else {
            panic!("valid claimed evidence must restore");
        };
        assert!(matches!(
            session.state(),
            AgentState::AwaitingApproval { .. }
        ));
        assert!(!valid_claim.exists(), "restored evidence must be consumed");

        let invalid_claim = dir.0.join("agent_session.invalid-claim.json");
        crate::snapshot_file::write_atomic_private(&invalid_claim, b"not json").unwrap();
        let SessionClaim::Quarantined { path, .. } =
            try_claim_session_file_with(&public, |_| Ok(invalid_claim.clone())).unwrap()
        else {
            panic!("invalid claimed evidence must be quarantined");
        };
        assert_eq!(path, invalid_claim);
        assert_eq!(std::fs::read(&path).unwrap(), b"not json");
    }

    #[test]
    fn a_valid_claim_is_synced_before_decode_and_after_consumption() {
        let dir = TestDir::new("claim-durable-order");
        let public = dir.0.join("agent_session.json");
        let claimed = dir.0.join("agent_session.claimed.json");
        write_snapshot_file(&claimed, &pending_snapshot()).unwrap();
        let namespace_states = std::cell::RefCell::new(Vec::new());

        let result = try_claim_session_file_with_sync(
            &public,
            |_| Ok(claimed.clone()),
            |path| {
                assert_eq!(path, claimed);
                namespace_states.borrow_mut().push(path.exists());
                Ok(())
            },
        )
        .unwrap();

        assert!(matches!(result, SessionClaim::Restored(_)));
        assert_eq!(
            namespace_states.borrow().as_slice(),
            [true, false],
            "the retired public name must sync while the claim exists, then the consumed claim must sync after unlink"
        );
    }

    #[test]
    fn a_failed_claim_durability_barrier_exposes_no_session_and_keeps_evidence() {
        let dir = TestDir::new("claim-sync-failure");
        let public = dir.0.join("agent_session.json");
        let claimed = dir.0.join("agent_session.claimed.json");
        write_snapshot_file(&claimed, &pending_snapshot()).unwrap();

        let error = try_claim_session_file_with_sync(
            &public,
            |_| Ok(claimed.clone()),
            |_| Err(std::io::Error::other("injected directory sync failure")),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(claimed.exists(), "unsynced evidence must not be consumed");
    }

    #[test]
    fn a_failed_post_unlink_sync_keeps_the_nonreplayable_live_session() {
        let dir = TestDir::new("claim-cleanup-sync-failure");
        let public = dir.0.join("agent_session.json");
        let claimed = dir.0.join("agent_session.claimed.json");
        write_snapshot_file(&claimed, &pending_snapshot()).unwrap();
        let sync_calls = std::cell::Cell::new(0);

        let result = try_claim_session_file_with_sync(
            &public,
            |_| Ok(claimed.clone()),
            |_| {
                sync_calls.set(sync_calls.get() + 1);
                if sync_calls.get() == 1 {
                    Ok(())
                } else {
                    Err(std::io::Error::other("injected cleanup sync failure"))
                }
            },
        )
        .unwrap();

        assert!(matches!(result, SessionClaim::Restored(_)));
        assert_eq!(sync_calls.get(), 2);
        assert!(!claimed.exists());
    }

    #[test]
    fn snapshot_files_round_trip_and_survive_bad_input() {
        let dir = TestDir::new("roundtrip");
        let path = dir.0.join("nested/agent_session.json");

        let snapshot = pending_snapshot();
        write_snapshot_file(&path, &snapshot).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let parent_mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(parent_mode, 0o700);
        }

        let restored = legacy_read_snapshot_file(&path).expect("snapshot reads back");
        let restored = AgentSession::restore(restored).unwrap();
        let expected = AgentSession::restore(snapshot).unwrap();
        assert_eq!(restored.transcript(), expected.transcript());

        // Corrupt files read as None instead of failing the caller.
        std::fs::write(&path, "not json").unwrap();
        assert!(legacy_read_snapshot_file(&path).is_none());

        legacy_remove_snapshot_file(&path);
        assert!(legacy_read_snapshot_file(&path).is_none());
        // Removing a missing file is fine.
        legacy_remove_snapshot_file(&path);
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn claiming_a_session_has_exactly_one_winner() {
        let dir = TestDir::new("claim");
        let path = dir.0.join("agent_session.json");
        write_snapshot_file(&path, &pending_snapshot()).unwrap();

        let SessionClaim::Restored(session) = try_claim_session_file(&path).unwrap() else {
            panic!("the first claim must restore the session");
        };
        assert!(matches!(
            session.state(),
            AgentState::AwaitingApproval { .. }
        ));
        // The snapshot is consumed, so a second opener finds nothing — and no
        // leftover claim file can be restored later.
        assert!(!path.exists());
        assert!(matches!(
            try_claim_session_file(&path).unwrap(),
            SessionClaim::Vacant
        ));
        assert!(std::fs::read_dir(&dir.0).unwrap().next().is_none());

        // Claiming a path that never existed is vacant, not an error.
        assert!(matches!(
            try_claim_session_file(&dir.0.join("missing.json")).unwrap(),
            SessionClaim::Vacant
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn simultaneous_session_claims_have_exactly_one_restored_winner() {
        use std::sync::{Arc, Barrier};

        let dir = TestDir::new("claim-concurrent");
        let path = dir.0.join("agent_session.json");
        write_snapshot_file(&path, &pending_snapshot()).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let workers = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    match try_claim_session_file(&path).unwrap() {
                        SessionClaim::Restored(session) => {
                            assert!(matches!(
                                session.state(),
                                AgentState::AwaitingApproval { .. }
                            ));
                            true
                        }
                        SessionClaim::Vacant => false,
                        SessionClaim::Quarantined { path, error } => {
                            panic!(
                                "a valid concurrent claim was quarantined at {}: {error}",
                                path.display()
                            )
                        }
                    }
                })
            })
            .collect::<Vec<_>>();

        barrier.wait();
        let restored = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|restored| *restored)
            .count();

        assert_eq!(restored, 1);
        assert!(!path.exists());
        assert!(std::fs::read_dir(&dir.0).unwrap().next().is_none());
    }

    #[test]
    fn a_non_file_claim_failure_never_reads_or_removes_the_object() {
        let dir = TestDir::new("claim-directory");
        let path = dir.0.join("agent_session.json");
        std::fs::create_dir(&path).unwrap();

        assert_eq!(
            try_claim_session_file(&path).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(matches!(
            legacy_claim_session_file(&path),
            SessionClaim::Vacant
        ));
        assert!(path.is_dir());
        assert!(std::fs::read_dir(&path).unwrap().next().is_none());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn a_symlink_claim_is_quarantined_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let dir = TestDir::new("claim-symlink");
        let path = dir.0.join("agent_session.json");
        let target = dir.0.join("outside.json");
        std::fs::write(&target, b"outside stays intact").unwrap();
        symlink(&target, &path).unwrap();

        let SessionClaim::Quarantined {
            path: quarantined, ..
        } = try_claim_session_file(&path).unwrap()
        else {
            panic!("the claimed symlink must remain invalid evidence");
        };

        assert!(!path.exists());
        assert!(std::fs::symlink_metadata(&quarantined)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"outside stays intact");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_claim_returns_vacant_without_blocking_or_removing_it() {
        use std::os::unix::fs::FileTypeExt;

        let dir = TestDir::new("claim-fifo");
        let path = dir.0.join("agent_session.json");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `name` is a NUL-terminated path that remains live for the call.
        let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        if made != 0 {
            return;
        }

        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            let typed_kind = try_claim_session_file(&worker_path)
                .expect_err("a FIFO must be rejected as a claim source")
                .kind();
            let legacy_vacant = matches!(
                legacy_claim_session_file(&worker_path),
                SessionClaim::Vacant
            );
            sender.send((typed_kind, legacy_vacant)).unwrap();
        });

        let (typed_kind, legacy_vacant) = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("claiming a FIFO must not wait for a writer");
        assert_eq!(typed_kind, std::io::ErrorKind::InvalidInput);
        assert!(legacy_vacant);
        worker.join().unwrap();
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_fifo());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn an_unusable_claim_is_quarantined_rather_than_deleted() {
        let dir = TestDir::new("quarantine");
        let path = dir.0.join("agent_session.json");

        for evidence in ["not json", r#"{"version":99}"#] {
            std::fs::write(&path, evidence).unwrap();
            let SessionClaim::Quarantined {
                path: quarantined, ..
            } = try_claim_session_file(&path).unwrap()
            else {
                panic!("invalid evidence must be quarantined");
            };
            assert!(!path.exists(), "the original name is claimed");
            assert_eq!(std::fs::read_to_string(&quarantined).unwrap(), evidence);
            // A quarantined file is never restored by a later opener.
            assert!(matches!(
                try_claim_session_file(&path).unwrap(),
                SessionClaim::Vacant
            ));
            std::fs::remove_file(&quarantined).unwrap();
        }

        // A snapshot that decodes but cannot become a session is evidence too.
        let mut value: serde_json::Value =
            serde_json::from_str(&pending_snapshot().to_json().unwrap()).unwrap();
        value["turns_used"] = serde_json::json!(u32::MAX);
        std::fs::write(&path, value.to_string()).unwrap();
        assert!(matches!(
            legacy_claim_session_file(&path),
            SessionClaim::Quarantined { .. }
        ));
    }

    #[test]
    fn oversized_and_corrupt_snapshots_still_fail_closed() {
        let dir = TestDir::new("invalid");
        let path = dir.0.join("agent_session.json");

        std::fs::write(&path, vec![b'x'; MAX_AGENT_SNAPSHOT_JSON_BYTES + 1]).unwrap();
        assert!(legacy_read_snapshot_file(&path).is_none());

        std::fs::write(&path, "not json").unwrap();
        assert!(legacy_read_snapshot_file(&path).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn legacy_predictable_staging_symlink_is_never_followed() {
        use std::os::unix::fs::symlink;

        let dir = TestDir::new("legacy-staging-symlink");
        let parent = dir.0.join("nested");
        std::fs::create_dir(&parent).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = parent.join("agent_session.json");
        let outside = dir.0.join("outside");
        std::fs::write(&outside, b"outside stays intact").unwrap();
        let legacy_staged = parent.join(format!(".agent_session.json.next.{}", std::process::id()));
        symlink(&outside, &legacy_staged).unwrap();

        write_snapshot_file(&path, &pending_snapshot()).unwrap();

        assert_eq!(std::fs::read(&outside).unwrap(), b"outside stays intact");
        assert!(std::fs::symlink_metadata(&legacy_staged)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(legacy_read_snapshot_file(&path).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn destination_symlink_is_replaced_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let dir = TestDir::new("destination-symlink");
        let path = dir.0.join("agent_session.json");
        let outside = dir.0.join("outside");
        std::fs::write(&outside, b"outside stays intact").unwrap();
        symlink(&outside, &path).unwrap();

        write_snapshot_file(&path, &pending_snapshot()).unwrap();

        assert_eq!(std::fs::read(&outside).unwrap(), b"outside stays intact");
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_file());
        assert!(legacy_read_snapshot_file(&path).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_loose_existing_snapshot_restores_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TestDir::new("loose-target");
        let path = dir.0.join("agent_session.json");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_snapshot_file(&path, &pending_snapshot()).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(legacy_read_snapshot_file(&path).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn fifo_snapshot_read_returns_none_promptly() {
        let dir = TestDir::new("fifo");
        let path = dir.0.join("agent_session.json");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `name` is a NUL-terminated path that remains alive for the call.
        let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        if made != 0 {
            // Some sandboxes forbid FIFO creation; the shared snapshot module's
            // non-regular-file tests still cover the rejection in that case.
            return;
        }

        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            let _ = sender.send(legacy_read_snapshot_file(&reader_path));
        });
        let result = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("FIFO read must not wait for a writer");
        assert!(result.is_none());
        reader.join().unwrap();
    }

    #[test]
    fn family_auto_approval_api_is_fail_closed() {
        for command in [
            "ls -la",
            "git status",
            "cat Cargo.toml",
            "hostname new-name",
            "tree -o /tmp/tree.txt",
        ] {
            assert!(
                !is_auto_approvable(command),
                "unexpected approval: {command}"
            );
        }
    }

    fn snapshot_with_json_mutation(
        mutate: impl FnOnce(&mut serde_json::Value),
    ) -> AgentSessionSnapshot {
        let mut value: serde_json::Value =
            serde_json::from_str(&pending_snapshot().to_json().unwrap()).unwrap();
        mutate(&mut value);
        AgentSessionSnapshot::from_json(&serde_json::to_string(&value).unwrap()).unwrap()
    }

    #[test]
    fn restore_rejects_duplicate_ids_before_approval_can_bind_to_the_wrong_command() {
        // The planted turn is Rejected, and the counter follows it, so the
        // document satisfies every transcript-shape rule that runs before the
        // id rule: a model action may follow a rejected proposal. Anything
        // else (an approved planted turn, say) is refused one rule earlier for
        // its shape, which would leave the id binding itself untested.
        let snapshot = snapshot_with_json_mutation(|value| {
            let proposal_id = value
                .pointer("/state/AwaitingApproval/proposal_id")
                .cloned()
                .unwrap();
            let transcript = value["transcript"].as_array_mut().unwrap();
            let pending_index = transcript
                .iter()
                .position(|turn| turn.get("AssistantProposed").is_some())
                .unwrap();
            transcript.insert(
                pending_index,
                serde_json::json!({
                    "AssistantProposed": {
                        "id": proposal_id,
                        "command": "rm -rf important-data",
                        "status": "Rejected"
                    }
                }),
            );
            value["turns_used"] = serde_json::json!(2);
        });

        assert!(matches!(
            AgentSession::restore(snapshot),
            Err(AgentSnapshotError::Invalid(reason)) if reason.contains("duplicated")
        ));
    }

    #[test]
    fn restore_rejects_multiple_pending_and_state_status_mismatches() {
        let multiple_pending = snapshot_with_json_mutation(|value| {
            value["transcript"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "AssistantProposed": {
                        "id": 2,
                        "command": "printf second",
                        "status": "Pending"
                    }
                }));
            value["state"] = serde_json::json!({"AwaitingApproval": {"proposal_id": 2}});
            value["next_proposal_id"] = serde_json::json!(3);
        });
        // Two pending proposals can no longer be spelled at all: a model
        // action must follow a user turn, an observation, a diagnostic, or a
        // REJECTED proposal, so the second one is refused for its position
        // before the multiple-pending rule is ever consulted. Strictly
        // stronger than the rule this used to name, and the reason string is
        // what the user reads, so it is the one pinned here.
        assert!(matches!(
            AgentSession::restore(multiple_pending),
            Err(AgentSnapshotError::Invalid(
                "model action does not follow a model-request boundary"
            ))
        ));

        let approved_but_awaiting_approval = snapshot_with_json_mutation(|value| {
            let proposal = value["transcript"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find_map(|turn| turn.get_mut("AssistantProposed"))
                .unwrap();
            proposal["status"] = serde_json::json!("Approved");
        });
        assert!(matches!(
            AgentSession::restore(approved_but_awaiting_approval),
            Err(AgentSnapshotError::Invalid(reason)) if reason.contains("approval state")
        ));

        let pending_but_ready = snapshot_with_json_mutation(|value| {
            value["state"] = serde_json::json!("Ready");
        });
        assert!(matches!(
            AgentSession::restore(pending_but_ready),
            Err(AgentSnapshotError::Invalid(reason)) if reason.contains("outside approval")
        ));

        let pending_but_awaiting_observation = snapshot_with_json_mutation(|value| {
            value["state"] = serde_json::json!({"AwaitingObservation": {"proposal_id": 1}});
        });
        assert!(matches!(
            AgentSession::restore(pending_but_awaiting_observation),
            Err(AgentSnapshotError::Invalid(reason)) if reason.contains("observation state")
        ));
    }

    /// The two answers core still gives that jagent's own restore does not.
    ///
    /// Both are asserted against jagent directly as well, because a check that
    /// only duplicates jagent's is worse than none: it runs first, so the
    /// weaker copy would be the one that always answers.
    #[test]
    fn the_family_budget_and_the_stale_counter_are_still_cores_own_answer() {
        // jagent bounds a transcript by the size of the PROMPT it reconstructs,
        // which is not the size of the document this family writes to disk and
        // reads back. JSON escaping is the whole gap: a message of quote
        // characters doubles on the wire and does not grow the prompt at all.
        let mut turns = Vec::new();
        for id in 1..=8 {
            turns.push(serde_json::json!({ "User": "\"".repeat(8 * 1024) }));
            turns.push(serde_json::json!({
                "AssistantProposed": {"command": "printf x", "id": id, "status": "Rejected"}
            }));
        }
        let encoded = serde_json::json!({
            "version": 1,
            "transcript": turns,
            "transcript_truncated": false,
            "state": "AwaitingModel",
            "turns_used": 8,
            "max_turns": 100,
            "next_proposal_id": 9,
        })
        .to_string();
        let wide = AgentSessionSnapshot::from_json(&encoded)
            .expect("jagent's own document ceiling accepts this");
        assert!(
            serde_json::to_vec(wide.transcript()).unwrap().len() > MAX_STORED_TRANSCRIPT_BYTES,
            "the fixture must actually exceed the family's stored-transcript budget"
        );
        assert!(
            jagent::session::AgentSession::restore(wide.clone()).is_ok(),
            "jagent restores this; the refusal below is this family's own budget"
        );
        assert!(matches!(
            AgentSession::restore(wide),
            Err(AgentSnapshotError::Invalid(
                "transcript exceeds its byte limit"
            ))
        ));

        // jagent REPAIRS a next proposal id that has fallen behind its own
        // transcript, by taking the maximum of the stored value and
        // `highest + 1`. A counter that disagrees with the transcript it was
        // saved beside is evidence the document was edited, and proposal ids
        // are what bind an approval card to the command handed back on
        // approval, so this family refuses the document instead.
        let stale = snapshot_with_json_mutation(|value| {
            value["next_proposal_id"] = serde_json::json!(1);
        });
        assert!(
            jagent::session::AgentSession::restore(stale.clone()).is_ok(),
            "jagent repairs the counter; the refusal below is this family's own"
        );
        assert!(matches!(
            AgentSession::restore(stale),
            Err(AgentSnapshotError::Invalid(
                "next proposal id is stale or exhausted"
            ))
        ));
    }

    /// A two-proposal pending snapshot: transcript [User, Proposed#1(Rejected),
    /// Proposed#2(Pending)], state AwaitingApproval{#2}.
    fn rejected_then_pending_snapshot_json() -> serde_json::Value {
        let mut session = AgentSession::new(6);
        session.submit_user("inspect").unwrap();
        let ModelOutcome::Proposal { id: first, .. } = session
            .accept_model_reply(r#"{"action":"run","command":"printf first"}"#)
            .unwrap()
        else {
            panic!("expected first proposal")
        };
        session.reject(first).unwrap();
        let ModelOutcome::Proposal { .. } = session
            .accept_model_reply(r#"{"action":"run","command":"printf second"}"#)
            .unwrap()
        else {
            panic!("expected second proposal")
        };
        serde_json::from_str(&session.snapshot().unwrap().to_json().unwrap()).unwrap()
    }

    fn decode_snapshot_json(value: &serde_json::Value) -> AgentSessionSnapshot {
        AgentSessionSnapshot::from_json(&serde_json::to_string(value).unwrap()).unwrap()
    }

    #[test]
    fn restore_rejects_a_pending_proposal_that_is_not_the_final_turn() {
        let base = rejected_then_pending_snapshot_json();
        assert!(AgentSession::restore(decode_snapshot_json(&base)).is_ok());

        // Hidden: the sole pending status sits on an older proposal behind a
        // newer final one. An approval card bound to it would authorize a
        // command the user never saw as current. The shape is now refused for
        // its position — nothing may follow a pending proposal, because the
        // model cannot be asked for anything while a card is open — so the
        // covering turn is rejected before the approval binding is examined.
        let mut hidden = base.clone();
        let mut seen = 0;
        for turn in hidden["transcript"].as_array_mut().unwrap() {
            if let Some(proposal) = turn.get_mut("AssistantProposed") {
                proposal["status"] =
                    serde_json::json!(if seen == 0 { "Pending" } else { "Rejected" });
                seen += 1;
            }
        }
        hidden["state"] = serde_json::json!({"AwaitingApproval": {"proposal_id": 1}});
        assert!(matches!(
            AgentSession::restore(decode_snapshot_json(&hidden)),
            Err(AgentSnapshotError::Invalid(
                "model action does not follow a model-request boundary"
            ))
        ));

        // Covered: the pending proposal is buried under a later turn, so the
        // visible final turn and the authorizable action disagree. A
        // diagnostic needs an outstanding model request or an approved
        // command's result to belong to, and a pending card is neither, so a
        // persisted diagnostic cannot manufacture a boundary that hides a
        // card.
        let mut covered = base;
        covered["transcript"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"ProtocolError": "cover pending proposal"}));
        assert!(matches!(
            AgentSession::restore(decode_snapshot_json(&covered)),
            Err(AgentSnapshotError::Invalid(
                "protocol error has no outstanding operation"
            ))
        ));
    }

    #[test]
    fn restore_rejects_an_unobserved_approved_proposal_outside_observation_state() {
        // The state machine only reaches Ready with a final AssistantSay,
        // ProtocolError, or manual-review proposal, so this buries an
        // unobserved approved proposal under a say: without
        // AwaitingObservation (whose restore normalization records an explicit
        // unknown-result note) the command's fate would be silently erased.
        // The burial is now refused for its shape — the only turns that may
        // follow an approved proposal are its observation or a diagnostic that
        // documents its result, both of which record the fate — so the
        // outcome rule this used to name has become unreachable rather than
        // optional.
        let mut buried = rejected_then_pending_snapshot_json();
        let mut seen = 0;
        for turn in buried["transcript"].as_array_mut().unwrap() {
            if let Some(proposal) = turn.get_mut("AssistantProposed") {
                proposal["status"] =
                    serde_json::json!(if seen == 0 { "Approved" } else { "Rejected" });
                seen += 1;
            }
        }
        buried["transcript"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"AssistantSay": "all done"}));
        buried["state"] = serde_json::json!("Ready");
        buried["turns_used"] = serde_json::json!(3);
        assert!(matches!(
            AgentSession::restore(decode_snapshot_json(&buried)),
            Err(AgentSnapshotError::Invalid(
                "model action does not follow a model-request boundary"
            ))
        ));

        // The same shape as the final turn in Ready fails the state/final-turn
        // match before the lifecycle rule is even reached.
        let mut final_unobserved = rejected_then_pending_snapshot_json();
        for turn in final_unobserved["transcript"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .rev()
        {
            if let Some(proposal) = turn.get_mut("AssistantProposed") {
                proposal["status"] = serde_json::json!("Approved");
                break;
            }
        }
        final_unobserved["state"] = serde_json::json!("Ready");
        assert!(matches!(
            AgentSession::restore(decode_snapshot_json(&final_unobserved)),
            Err(AgentSnapshotError::Invalid(reason)) if reason.contains("final transcript turn")
        ));
    }

    /// A snapshot whose approved proposal was observed: transcript [User,
    /// Proposed#1(Approved), Observation], state AwaitingModel.
    fn observed_snapshot_json() -> serde_json::Value {
        let mut session = AgentSession::new(6);
        session.submit_user("inspect").unwrap();
        let ModelOutcome::Proposal { id, .. } = session
            .accept_model_reply(r#"{"action":"run","command":"printf safe"}"#)
            .unwrap()
        else {
            panic!("expected proposal")
        };
        let _approved = session.approve(id).unwrap();
        session.observe(id, 0, "safe output").unwrap();
        serde_json::from_str(&session.snapshot().unwrap().to_json().unwrap()).unwrap()
    }

    fn execution_failure_snapshot_json(
        failure: CommandExecutionFailure,
        detail: &str,
    ) -> serde_json::Value {
        let mut session = AgentSession::new(6);
        session.submit_user("inspect").unwrap();
        let ModelOutcome::Proposal { id, .. } = session
            .accept_model_reply(r#"{"action":"run","command":"check"}"#)
            .unwrap()
        else {
            panic!("expected proposal")
        };
        let _approved = session.approve(id).unwrap();
        session
            .observe_execution_failure(id, failure, detail)
            .unwrap();
        serde_json::from_str(&session.snapshot().unwrap().to_json().unwrap()).unwrap()
    }

    fn final_protocol_error_mut(value: &mut serde_json::Value) -> &mut serde_json::Value {
        value["transcript"]
            .as_array_mut()
            .and_then(|turns| turns.last_mut())
            .and_then(|turn| turn.get_mut("ProtocolError"))
            .expect("fixture must end in a protocol diagnostic")
    }

    #[test]
    fn explicit_execution_failure_snapshots_round_trip_for_every_07_reason() {
        for (failure, detail) in [
            (
                CommandExecutionFailure::FailedToStart,
                "executable not found",
            ),
            (
                CommandExecutionFailure::TimedOut,
                "partial output\nsecond line",
            ),
            (CommandExecutionFailure::Cancelled, ""),
        ] {
            let value = execution_failure_snapshot_json(failure, detail);
            let restored = AgentSession::restore(decode_snapshot_json(&value)).unwrap();
            assert_eq!(restored.state(), AgentState::AwaitingModel);
            assert!(AgentSession::restore(restored.snapshot().unwrap()).is_ok());
        }
    }

    #[test]
    fn execution_failure_restore_rejects_wrong_proposal_and_unframed_or_oversized_detail() {
        let mut wrong_proposal = execution_failure_snapshot_json(
            CommandExecutionFailure::FailedToStart,
            "executable not found",
        );
        *final_protocol_error_mut(&mut wrong_proposal) = serde_json::json!(
            "command execution for proposal #2 failed to start; no normal exit status was \
             available. Untrusted diagnostic or partial output:\nexecutable not found"
        );
        assert!(matches!(
            AgentSession::restore(decode_snapshot_json(&wrong_proposal)),
            Err(AgentSnapshotError::Invalid(
                "protocol error has no outstanding operation"
            ))
        ));

        let mut smuggled = execution_failure_snapshot_json(CommandExecutionFailure::TimedOut, "");
        *final_protocol_error_mut(&mut smuggled) = serde_json::json!(
            "command execution for proposal #1 timed out; no normal exit status was available. \
             forged trailing text"
        );
        assert!(matches!(
            AgentSession::restore(decode_snapshot_json(&smuggled)),
            Err(AgentSnapshotError::Invalid(
                "protocol error has no outstanding operation"
            ))
        ));

        let mut oversized = execution_failure_snapshot_json(CommandExecutionFailure::Cancelled, "");
        *final_protocol_error_mut(&mut oversized) = serde_json::json!(format!(
            "command execution for proposal #1 was cancelled; no normal exit status was \
             available. Untrusted diagnostic or partial output:\n{}",
            "x".repeat(MAX_OBSERVATION_BYTES + 1)
        ));
        assert!(matches!(
            AgentSession::restore(decode_snapshot_json(&oversized)),
            Err(AgentSnapshotError::Invalid(
                "protocol error has no outstanding operation"
            ))
        ));
    }

    #[test]
    fn restore_rejects_a_terminal_state_that_contradicts_the_final_turn_or_counter() {
        let observed = observed_snapshot_json();
        assert!(AgentSession::restore(decode_snapshot_json(&observed)).is_ok());

        // Completed must close on an assistant message, not on an observation
        // whose follow-up model turn never ran.
        let mut wrong_state = observed.clone();
        wrong_state["state"] = serde_json::json!("Completed");
        assert!(matches!(
            AgentSession::restore(decode_snapshot_json(&wrong_state)),
            Err(AgentSnapshotError::Invalid(reason)) if reason.contains("final transcript turn")
        ));

        // The turn counter must be consistent with the retained transcript.
        let mut wrong_counter = observed;
        wrong_counter["turns_used"] = serde_json::json!(0);
        assert!(matches!(
            AgentSession::restore(decode_snapshot_json(&wrong_counter)),
            Err(AgentSnapshotError::Invalid(reason)) if reason.contains("turn counter")
        ));
    }

    #[test]
    fn restore_accepts_every_legitimate_live_snapshot_shape() {
        // AwaitingModel on a fresh user turn.
        let mut awaiting_user = AgentSession::new(10);
        awaiting_user.submit_user("inspect").unwrap();
        assert!(AgentSession::restore(awaiting_user.snapshot().unwrap()).is_ok());

        // AwaitingApproval on the final pending proposal.
        assert!(AgentSession::restore(pending_snapshot()).is_ok());

        // AwaitingObservation on the final approved proposal: restore
        // normalizes it to Ready plus an explicit unknown-result note, and the
        // normalized session itself snapshots into a restorable shape.
        let mut in_flight = AgentSession::new(10);
        in_flight.submit_user("inspect").unwrap();
        let ModelOutcome::Proposal { id, .. } = in_flight
            .accept_model_reply(r#"{"action":"run","command":"printf safe"}"#)
            .unwrap()
        else {
            panic!("expected proposal")
        };
        let _approved = in_flight.approve(id).unwrap();
        let snapshot = in_flight.snapshot().unwrap();
        assert!(matches!(
            snapshot.state(),
            AgentState::AwaitingObservation { .. }
        ));
        let restored = AgentSession::restore(snapshot).unwrap();
        assert_eq!(restored.state(), AgentState::Ready);
        assert!(matches!(
            restored.transcript().last(),
            Some(Turn::ProtocolError(note)) if note.contains("unknown result")
        ));
        assert!(AgentSession::restore(restored.snapshot().unwrap()).is_ok());

        // AwaitingModel on a rejection, an observation, Ready on a say, a
        // protocol error, and a manual review, and Completed on a done.
        let mut session = AgentSession::new(10);
        session.submit_user("inspect").unwrap();
        let ModelOutcome::Proposal { id, .. } = session
            .accept_model_reply(r#"{"action":"run","command":"printf first"}"#)
            .unwrap()
        else {
            panic!("expected proposal")
        };
        session.reject(id).unwrap();
        assert!(matches!(session.state(), AgentState::AwaitingModel));
        assert!(AgentSession::restore(session.snapshot().unwrap()).is_ok());

        let ModelOutcome::Proposal { id, .. } = session
            .accept_model_reply(r#"{"action":"run","command":"printf second"}"#)
            .unwrap()
        else {
            panic!("expected proposal")
        };
        let _approved = session.approve(id).unwrap();
        session.observe(id, 0, "output").unwrap();
        assert!(matches!(session.state(), AgentState::AwaitingModel));
        assert!(AgentSession::restore(session.snapshot().unwrap()).is_ok());

        session
            .accept_model_reply(r#"{"action":"say","message":"noted"}"#)
            .unwrap();
        assert_eq!(session.state(), AgentState::Ready);
        assert!(AgentSession::restore(session.snapshot().unwrap()).is_ok());

        session.submit_user("again").unwrap();
        assert!(session.accept_model_reply("not json").is_err());
        assert_eq!(session.state(), AgentState::Ready);
        assert!(AgentSession::restore(session.snapshot().unwrap()).is_ok());

        session.retry_model().unwrap();
        let ModelOutcome::Proposal { id, .. } = session
            .accept_model_reply(r#"{"action":"run","command":"printf third"}"#)
            .unwrap()
        else {
            panic!("expected proposal")
        };
        session.edit_for_manual_review(id, "printf third").unwrap();
        assert_eq!(session.state(), AgentState::Ready);
        assert!(AgentSession::restore(session.snapshot().unwrap()).is_ok());

        session.submit_user("finish").unwrap();
        session
            .accept_model_reply(r#"{"action":"done","message":"done"}"#)
            .unwrap();
        assert_eq!(session.state(), AgentState::Completed);
        assert!(AgentSession::restore(session.snapshot().unwrap()).is_ok());

        // TurnLimitReached on each final-turn shape the limit can produce.
        let mut limited = AgentSession::new(1);
        limited.submit_user("once").unwrap();
        limited
            .accept_model_reply(r#"{"action":"say","message":"only turn"}"#)
            .unwrap();
        assert_eq!(limited.state(), AgentState::TurnLimitReached);
        assert!(AgentSession::restore(limited.snapshot().unwrap()).is_ok());

        let mut limited_reject = AgentSession::new(1);
        limited_reject.submit_user("once").unwrap();
        let ModelOutcome::Proposal { id, .. } = limited_reject
            .accept_model_reply(r#"{"action":"run","command":"printf once"}"#)
            .unwrap()
        else {
            panic!("expected proposal")
        };
        limited_reject.reject(id).unwrap();
        assert_eq!(limited_reject.state(), AgentState::TurnLimitReached);
        assert!(AgentSession::restore(limited_reject.snapshot().unwrap()).is_ok());

        let mut limited_observe = AgentSession::new(1);
        limited_observe.submit_user("once").unwrap();
        let ModelOutcome::Proposal { id, .. } = limited_observe
            .accept_model_reply(r#"{"action":"run","command":"printf once"}"#)
            .unwrap()
        else {
            panic!("expected proposal")
        };
        let _approved = limited_observe.approve(id).unwrap();
        limited_observe.observe(id, 0, "output").unwrap();
        assert_eq!(limited_observe.state(), AgentState::TurnLimitReached);
        assert!(AgentSession::restore(limited_observe.snapshot().unwrap()).is_ok());

        let mut limited_error = AgentSession::new(1);
        limited_error.submit_user("once").unwrap();
        assert!(limited_error.accept_model_reply("not json").is_err());
        assert_eq!(limited_error.state(), AgentState::TurnLimitReached);
        assert!(AgentSession::restore(limited_error.snapshot().unwrap()).is_ok());

        let mut limited_review = AgentSession::new(1);
        limited_review.submit_user("once").unwrap();
        let ModelOutcome::Proposal { id, .. } = limited_review
            .accept_model_reply(r#"{"action":"run","command":"printf once"}"#)
            .unwrap()
        else {
            panic!("expected proposal")
        };
        limited_review
            .edit_for_manual_review(id, "printf once")
            .unwrap();
        assert_eq!(limited_review.state(), AgentState::TurnLimitReached);
        assert!(AgentSession::restore(limited_review.snapshot().unwrap()).is_ok());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn a_group_readable_claimed_snapshot_is_quarantined_as_tampering() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TestDir::new("claim-loose-permissions");
        let path = dir.0.join("agent_session.json");
        write_snapshot_file(&path, &pending_snapshot()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        let SessionClaim::Quarantined {
            path: quarantined,
            error,
        } = try_claim_session_file(&path).unwrap()
        else {
            panic!("loose permissions on claimed evidence must quarantine it");
        };
        assert!(matches!(error, AgentSnapshotError::Decode(_)));
        assert!(!path.exists(), "the original name is claimed");
        // The evidence survives for inspection and is never restored.
        assert!(
            AgentSessionSnapshot::from_json(&std::fs::read_to_string(&quarantined).unwrap())
                .is_ok()
        );
        assert!(matches!(
            try_claim_session_file(&path).unwrap(),
            SessionClaim::Vacant
        ));
        std::fs::remove_file(&quarantined).unwrap();
    }

    #[test]
    fn task_and_session_replacement_advance_the_callback_epoch() {
        let mut session = AgentSession::new(4);
        let initial = session.epoch();
        session.submit_user("finish").unwrap();
        session
            .accept_model_reply(r#"{"action":"done","message":"done"}"#)
            .unwrap();
        session.start_new_task().unwrap();
        assert_ne!(session.epoch(), initial);
        assert!(!session.is_current_epoch(initial));

        let replacement = AgentSession::new(4);
        assert_ne!(replacement.epoch(), session.epoch());
    }

    #[test]
    fn live_text_and_tool_proposals_reject_visual_spoofing_centrally() {
        let mut text_session = AgentSession::new(4);
        text_session.submit_user("inspect").unwrap();
        let error = text_session
            .accept_model_reply(
                &serde_json::json!({
                    "action": "run",
                    "command": "printf safe\u{202e}; rm -rf important"
                })
                .to_string(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            SessionError::Protocol(ParseError::InvalidCommand(_))
        ));
        assert_eq!(text_session.state(), AgentState::Ready);
        assert!(text_session.transcript().iter().all(|turn| !matches!(
            turn,
            Turn::AssistantProposed {
                status: ProposalStatus::Pending,
                ..
            }
        )));

        let mut tool_session = AgentSession::new(4);
        tool_session.submit_user("inspect").unwrap();
        let reply = jagent::tools::ToolResponse::new(
            "",
            vec![jagent::tools::ToolCall {
                id: "call-1".into(),
                name: jagent::tools::TOOL_RUN.into(),
                arguments: serde_json::json!({
                    "command": "printf safe\u{00a0}; rm -rf important"
                })
                .to_string(),
            }],
        );
        let error = tool_session.accept_model_tool_reply(&reply).unwrap_err();
        assert!(matches!(
            error,
            SessionError::Protocol(ParseError::InvalidCommand(_))
        ));
        assert_eq!(tool_session.state(), AgentState::Ready);
    }

    /// The command rule is jagent's; this module only names its reasons.
    ///
    /// Every reason the family shipped before delegation still has to come
    /// back for the same input, and the one message that quotes a number has
    /// to quote the number jagent actually enforces.
    #[test]
    fn the_command_contract_is_jagents_and_every_family_reason_survives_it() {
        assert_eq!(
            MAX_COMMAND_BYTES,
            16 * 1024,
            "the TooLarge reason spells this limit out, so it cannot drift silently"
        );
        for (command, reason) in [
            ("   ", "command must not be empty"),
            (
                &"x".repeat(MAX_COMMAND_BYTES + 1),
                "command exceeds the 16384-byte safety limit",
            ),
            (
                "printf one\nprintf two",
                "command contains a control character",
            ),
            ("printf \u{1b}[31m", "command contains a control character"),
            (
                "printf safe\u{202e}gnp.exe",
                "command contains invisible or bidirectional formatting",
            ),
        ] {
            assert_eq!(validate_agent_command(command), Err(reason), "{command:?}");
        }
        assert_eq!(validate_agent_command("printf safe"), Ok(()));
        // The rule really is jagent's, not a copy that agrees today.
        for command in [
            "   ",
            "printf one\nprintf two",
            "printf \u{1b}[31m",
            "printf safe\u{202e}gnp.exe",
            "printf safe\u{e0000}",
            "printf safe",
        ] {
            assert_eq!(
                validate_agent_command(command).is_ok(),
                validate_command_text(command).is_ok(),
                "{command:?} forks jagent's command-text contract"
            );
        }
    }

    #[test]
    fn edited_approval_and_manual_review_share_the_hidden_text_boundary() {
        for manual_review in [false, true] {
            let mut session = AgentSession::new(4);
            session.submit_user("inspect").unwrap();
            let ModelOutcome::Proposal { id, .. } = session
                .accept_model_reply(r#"{"action":"run","command":"printf safe"}"#)
                .unwrap()
            else {
                panic!("expected proposal")
            };

            let result = if manual_review {
                session
                    .edit_for_manual_review(id, "printf safe\u{2066}hidden")
                    .map(|_| ())
            } else {
                session
                    .edit_and_approve(id, "printf safe\u{2066}hidden")
                    .map(|_| ())
            };
            assert!(matches!(
                result,
                Err(SessionError::Protocol(ParseError::InvalidCommand(_)))
            ));
            assert_eq!(
                session.state(),
                AgentState::AwaitingApproval { proposal_id: id }
            );
            assert!(session.transcript().iter().any(|turn| matches!(
                turn,
                Turn::AssistantProposed {
                    id: candidate,
                    status: ProposalStatus::Pending,
                    ..
                } if *candidate == id
            )));
        }
    }

    #[test]
    fn prepared_request_keeps_provider_protocol_and_session_ingestion_bound() {
        let history = [jagent::Message {
            role: jagent::Role::User,
            text: "inspect".into(),
        }];
        let config = jagent::ChatConfig {
            provider: jagent::Provider::OpenAiCompatible,
            api_key: None,
            model: "local-test".into(),
            base_url: "http://127.0.0.1:11434/v1".into(),
            max_tokens: 128,
            temperature: Some(0.0),
        };
        let prepared = prepare_agent_request(
            &config,
            AgentRequestSpec::new(&history, AgentProtocol::Text),
        )
        .unwrap();
        assert_eq!(prepared.protocol(), AgentProtocol::Text);
        assert!(prepared.report.redaction_enabled);

        let response = prepared
            .parse_response(
                br#"{"choices":[{"message":{"content":"{\"action\":\"run\",\"command\":\"pwd\"}"},"finish_reason":"stop"}]}"#,
            )
            .unwrap();
        let mut session = AgentSession::new(4);
        session.submit_user("inspect").unwrap();
        let ModelOutcome::Proposal { command, .. } =
            session.accept_agent_response(&response).unwrap()
        else {
            panic!("expected proposal")
        };
        assert_eq!(command, "pwd");
    }

    #[test]
    fn capability_negotiation_is_available_through_the_core_facade() {
        assert_eq!(AGENT_CAPABILITIES_VERSION, 2);
        assert!(AGENT_CAPABILITIES_V1_WIRE.len() <= MAX_AGENT_CAPABILITIES_WIRE_BYTES);
        assert!(AGENT_CAPABILITIES_V2_WIRE.len() <= MAX_AGENT_CAPABILITIES_WIRE_BYTES);

        let peer = AgentCapabilities::from_wire("jagent-agent/1;protocols=text;delivery=complete")
            .unwrap();
        assert!(peer.supports(AgentProtocol::Text, AgentDelivery::Complete));
        assert!(!peer.supports(AgentProtocol::NativeTools, AgentDelivery::Complete));
        assert_eq!(
            AgentCapabilities::from_wire("not-a-capability-token"),
            Err(CapabilityError::Malformed)
        );

        let local = agent_capabilities(AgentProvider::OpenAiCompatible);
        assert_eq!(local.version(), 1);
        assert_eq!(
            local.negotiate_with(
                peer,
                &[AgentProtocol::NativeTools, AgentProtocol::Text],
                AgentDelivery::Complete,
            ),
            Some(AgentProtocol::Text)
        );

        let v2_peer = AgentCapabilities::from_wire(AGENT_CAPABILITIES_V2_WIRE).unwrap();
        assert_eq!(
            agent_capabilities_v2(AgentProvider::OpenAiCompatible).version(),
            2
        );
        assert_eq!(
            agent_capabilities_for_peer(AgentProvider::OpenAiCompatible, v2_peer).version(),
            2
        );
    }

    #[test]
    fn typed_execution_outcomes_cross_the_hardened_facade_and_restore() {
        for outcome in [
            CommandExecutionOutcome::exited(17, "real output"),
            CommandExecutionOutcome::failed(
                CommandExecutionFailure::FailedToStart,
                "spawn boundary refused the child",
            ),
        ] {
            let expected_exit = outcome.exit_code();
            let expected_failure = outcome.failure();
            let mut session = AgentSession::new(4);
            session.submit_user("inspect").unwrap();
            let ModelOutcome::Proposal { id, .. } = session
                .accept_model_reply(r#"{"action":"run","command":"check"}"#)
                .unwrap()
            else {
                panic!("expected proposal")
            };
            let _approved = session.approve(id).unwrap();
            session.observe_execution(id, outcome).unwrap();

            match (expected_exit, expected_failure, session.transcript().last()) {
                (Some(17), None, Some(Turn::Observation { exit_code: 17, .. })) => {}
                (
                    None,
                    Some(CommandExecutionFailure::FailedToStart),
                    Some(Turn::ProtocolError(message)),
                ) if message.contains("no normal exit status") => {}
                unexpected => panic!("unexpected execution transcript: {unexpected:?}"),
            }
            let snapshot = session.snapshot().unwrap();
            assert!(AgentSession::restore(snapshot).is_ok());
        }
    }
}
