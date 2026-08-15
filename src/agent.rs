//! Review-first agent core, shared with jsh through the `jagent` crate.
//!
//! The pure session state machine, action parser, safety heuristics, and
//! snapshot serialization live in `jagent` (sans-IO). This module re-exports
//! that surface under the historical `jterm_core::agent` paths and adds the
//! filesystem persistence helpers the jterm apps use for
//! `<config-dir>/<app>/agent_session.json`.

pub use jagent::safety::is_dangerous;
pub use jagent::session::{
    parse_action, sample_observation, AgentSessionSnapshot, AgentSnapshotError, AgentState,
    ApprovedCommand, CancellationToken, ModelOutcome, ParseError, ParsedAction, ProposalId,
    ProposalStatus, SessionError, Turn, MAX_AGENT_SNAPSHOT_JSON_BYTES,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_SESSION_TURNS: u32 = 1_000;
const MAX_STORED_TRANSCRIPT_ENTRIES: usize = 128;
const MAX_STORED_TRANSCRIPT_BYTES: usize = 128 * 1024;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
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

    pub fn cancel(&mut self) {
        self.inner.cancel();
    }

    pub fn build_user_prompt(&self) -> String {
        self.inner.build_user_prompt()
    }

    pub fn build_user_prompt_with(&self, protocol: jagent::tools::AgentProtocol) -> String {
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

    let mut highest_proposal_id = 0_u64;
    let mut proposal_statuses: HashMap<u64, (ProposalStatus, usize)> = HashMap::new();
    let mut pending_proposal = None;
    let mut observed_proposals = HashSet::new();
    let mut model_actions = 0_u32;
    let mut protocol_errors = 0_u32;
    for (index, turn) in transcript.iter().enumerate() {
        match turn {
            Turn::User(message) => {
                validate_snapshot_text(message, MAX_MESSAGE_BYTES, true)?;
            }
            Turn::AssistantSay(message) => {
                validate_snapshot_text(message, MAX_MESSAGE_BYTES, true)?;
                model_actions = model_actions.saturating_add(1);
            }
            Turn::AssistantThought(thought) => {
                validate_snapshot_text(thought, MAX_THOUGHT_BYTES, true)?;
            }
            Turn::AssistantProposed {
                id,
                command,
                status,
            } => {
                model_actions = model_actions.saturating_add(1);
                let id_value = id.get();
                if id_value == 0 || id_value <= highest_proposal_id {
                    return Err(AgentSnapshotError::Invalid(
                        "proposal ids are zero, duplicated, or out of order",
                    ));
                }
                validate_snapshot_command(command)?;
                highest_proposal_id = id_value;
                proposal_statuses.insert(id_value, (*status, index));
                if *status == ProposalStatus::Pending
                    && pending_proposal.replace((*id, index)).is_some()
                {
                    return Err(AgentSnapshotError::Invalid(
                        "snapshot contains multiple pending proposals",
                    ));
                }
            }
            Turn::Observation {
                proposal_id,
                output_sample,
                ..
            } => {
                if proposal_id.get() == 0 || output_sample.len() > MAX_OBSERVATION_BYTES {
                    return Err(AgentSnapshotError::Invalid(
                        "observation violates its safety bounds",
                    ));
                }
                if proposal_statuses
                    .get(&proposal_id.get())
                    .map(|(status, _)| status)
                    != Some(&ProposalStatus::Approved)
                    || !observed_proposals.insert(proposal_id.get())
                {
                    return Err(AgentSnapshotError::Invalid(
                        "observation does not identify one approved proposal",
                    ));
                }
            }
            Turn::ProtocolError(message) => {
                validate_snapshot_text(message, MAX_MESSAGE_BYTES, false)?;
                protocol_errors = protocol_errors.saturating_add(1);
            }
        }
    }

    if snapshot.next_proposal_id() == 0
        || snapshot.next_proposal_id() == u64::MAX
        || snapshot.next_proposal_id() <= highest_proposal_id
    {
        return Err(AgentSnapshotError::Invalid(
            "next proposal id is stale or exhausted",
        ));
    }

    // Every retained proposal/say consumed one model turn. A ProtocolError may
    // be either a parse failure (one turn) or a transport failure (no turn),
    // which gives an exact range while the transcript is untruncated.
    let turns_used = snapshot.turns_used();
    if turns_used < model_actions
        || (!snapshot.transcript_truncated()
            && turns_used > model_actions.saturating_add(protocol_errors))
    {
        return Err(AgentSnapshotError::Invalid(
            "turn counter is inconsistent with the transcript",
        ));
    }

    // An empty transcript is rejected by jagent's own restore below; every
    // remaining check reasons about the final turn, so there is nothing more
    // to audit on one here.
    let Some(final_index) = transcript.len().checked_sub(1) else {
        return Ok(());
    };
    let final_turn = &transcript[final_index];

    // The state must bind to the transcript's final turn: an approval card
    // (or an in-flight execution) anywhere else would split the reviewed UI
    // identity from the session's authorizable action.
    match snapshot.state() {
        AgentState::AwaitingApproval { proposal_id }
            if pending_proposal == Some((proposal_id, final_index)) => {}
        AgentState::AwaitingApproval { .. } => {
            return Err(AgentSnapshotError::Invalid(
                "approval state does not identify the final pending proposal",
            ));
        }
        AgentState::AwaitingObservation { proposal_id }
            if pending_proposal.is_none()
                && proposal_statuses.get(&proposal_id.get())
                    == Some(&(ProposalStatus::Approved, final_index))
                && !observed_proposals.contains(&proposal_id.get()) => {}
        AgentState::AwaitingObservation { .. } => {
            return Err(AgentSnapshotError::Invalid(
                "observation state does not identify the final unobserved approved proposal",
            ));
        }
        _ if pending_proposal.is_some() => {
            return Err(AgentSnapshotError::Invalid(
                "pending proposal exists outside approval state",
            ));
        }
        _ => {}
    }

    // Terminal and waiting states must match the final turn's shape exactly
    // the way the live transitions produce it.
    let final_state_is_valid = match snapshot.state() {
        AgentState::Ready => {
            turns_used < snapshot.max_turns()
                && matches!(
                    final_turn,
                    Turn::AssistantSay(_)
                        | Turn::ProtocolError(_)
                        | Turn::AssistantProposed {
                            status: ProposalStatus::ManualReview,
                            ..
                        }
                )
        }
        AgentState::AwaitingModel => {
            turns_used < snapshot.max_turns()
                && matches!(
                    final_turn,
                    Turn::User(_)
                        | Turn::ProtocolError(_)
                        | Turn::Observation { .. }
                        | Turn::AssistantProposed {
                            status: ProposalStatus::Rejected,
                            ..
                        }
                )
        }
        // Both in-flight states were pinned to the final turn above.
        AgentState::AwaitingApproval { .. } | AgentState::AwaitingObservation { .. } => true,
        AgentState::Completed => matches!(final_turn, Turn::AssistantSay(_)),
        AgentState::TurnLimitReached => {
            turns_used == snapshot.max_turns()
                && matches!(
                    final_turn,
                    Turn::AssistantSay(_)
                        | Turn::ProtocolError(_)
                        | Turn::Observation { .. }
                        | Turn::AssistantProposed {
                            status: ProposalStatus::Rejected | ProposalStatus::ManualReview,
                            ..
                        }
                )
        }
        AgentState::Cancelled => false,
    };
    if !final_state_is_valid {
        return Err(AgentSnapshotError::Invalid(
            "session state does not match the final transcript turn or budget",
        ));
    }

    // An approved proposal's fate must be recorded: either its observation is
    // in the transcript, or it is the one in-flight execution the
    // AwaitingObservation state names, or — the shape jagent's
    // AwaitingObservation restore normalization produces — its unknown result
    // is documented by the note that normalization appended immediately after
    // it. Anything else silently erases the command's outcome.
    for (proposal_id, (status, index)) in &proposal_statuses {
        if *status != ProposalStatus::Approved {
            continue;
        }
        let is_current_unobserved = matches!(
            snapshot.state(),
            AgentState::AwaitingObservation {
                proposal_id: current
            } if current.get() == *proposal_id
        );
        if observed_proposals.contains(proposal_id) {
            // The state arm above already proved the AwaitingObservation
            // proposal is unobserved, so an observed proposal can never be the
            // in-flight one.
            if is_current_unobserved {
                return Err(AgentSnapshotError::Invalid(
                    "approved proposal observation lifecycle is inconsistent",
                ));
            }
            continue;
        }
        if !is_current_unobserved {
            let documented = Turn::ProtocolError(format!(
                "the application exited before proposal #{proposal_id}'s output was \
                 observed; its result is unknown"
            ));
            if transcript.get(index + 1) != Some(&documented) {
                return Err(AgentSnapshotError::Invalid(
                    "approved proposal observation lifecycle is inconsistent",
                ));
            }
        }
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

fn validate_snapshot_command(command: &str) -> Result<(), AgentSnapshotError> {
    validate_agent_command(command)
        .map_err(|_| AgentSnapshotError::Invalid("proposal command violates its safety bounds"))
}

fn validate_agent_command(command: &str) -> Result<(), &'static str> {
    if command.trim().is_empty() {
        return Err("command must not be empty");
    }
    if command.len() > MAX_COMMAND_BYTES {
        return Err("command exceeds the 16384-byte safety limit");
    }
    if command.chars().any(char::is_control) {
        return Err("command contains a control character");
    }
    if crate::review_input::contains_visual_spoofing(command) {
        return Err("command contains invisible or bidirectional formatting");
    }
    Ok(())
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
pub fn read_snapshot_file(path: &std::path::Path) -> Option<AgentSessionSnapshot> {
    let encoded =
        crate::snapshot_file::read_bounded(path, MAX_AGENT_SNAPSHOT_JSON_BYTES as u64).ok()?;
    AgentSessionSnapshot::from_json(&encoded).ok()
}

/// Remove a persisted snapshot; missing files are fine.
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
/// observes it, and the claim is only deleted once a session exists.
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
    let claimed = match claim(path) {
        Ok(claimed) => claimed,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Missing file or a lost race: do not fall back to reading the
            // public name after another opener may have retired it.
            return Ok(SessionClaim::Vacant);
        }
        Err(error) => return Err(error),
    };
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
            let _ = std::fs::remove_file(&claimed);
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
pub fn claim_session_file(path: &std::path::Path) -> SessionClaim {
    collapse_claim_result(path, try_claim_session_file(path))
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let restored = read_snapshot_file(&path).expect("snapshot reads back");
        let restored = AgentSession::restore(restored).unwrap();
        let expected = AgentSession::restore(snapshot).unwrap();
        assert_eq!(restored.transcript(), expected.transcript());

        // Corrupt files read as None instead of failing the caller.
        std::fs::write(&path, "not json").unwrap();
        assert!(read_snapshot_file(&path).is_none());

        remove_snapshot_file(&path);
        assert!(read_snapshot_file(&path).is_none());
        // Removing a missing file is fine.
        remove_snapshot_file(&path);
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
        assert!(matches!(claim_session_file(&path), SessionClaim::Vacant));
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
            let legacy_vacant = matches!(claim_session_file(&worker_path), SessionClaim::Vacant);
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
            claim_session_file(&path),
            SessionClaim::Quarantined { .. }
        ));
    }

    #[test]
    fn oversized_and_corrupt_snapshots_still_fail_closed() {
        let dir = TestDir::new("invalid");
        let path = dir.0.join("agent_session.json");

        std::fs::write(&path, vec![b'x'; MAX_AGENT_SNAPSHOT_JSON_BYTES + 1]).unwrap();
        assert!(read_snapshot_file(&path).is_none());

        std::fs::write(&path, "not json").unwrap();
        assert!(read_snapshot_file(&path).is_none());
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
        assert!(read_snapshot_file(&path).is_some());
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
        assert!(read_snapshot_file(&path).is_some());
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
        assert!(read_snapshot_file(&path).is_some());
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
            let _ = sender.send(read_snapshot_file(&reader_path));
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
                        "status": "Approved"
                    }
                }),
            );
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
        assert!(matches!(
            AgentSession::restore(multiple_pending),
            Err(AgentSnapshotError::Invalid(reason)) if reason.contains("multiple pending")
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
        // command the user never saw as current.
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
            Err(AgentSnapshotError::Invalid(reason)) if reason.contains("approval state")
        ));

        // Covered: the pending proposal is buried under a later turn, so the
        // visible final turn and the authorizable action disagree.
        let mut covered = base;
        covered["transcript"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"ProtocolError": "cover pending proposal"}));
        assert!(matches!(
            AgentSession::restore(decode_snapshot_json(&covered)),
            Err(AgentSnapshotError::Invalid(reason)) if reason.contains("approval state")
        ));
    }

    #[test]
    fn restore_rejects_an_unobserved_approved_proposal_outside_observation_state() {
        // The state machine only reaches Ready with a final AssistantSay,
        // ProtocolError, or manual-review proposal, so burying an unobserved
        // approved proposal under a say keeps every other check satisfied and
        // isolates the lifecycle rule: without AwaitingObservation (whose
        // restore normalization records an explicit unknown-result note) the
        // command's fate would be silently erased.
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
            Err(AgentSnapshotError::Invalid(reason)) if reason.contains("observation lifecycle")
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
            Some(Turn::ProtocolError(note)) if note.contains("result is unknown")
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
}
