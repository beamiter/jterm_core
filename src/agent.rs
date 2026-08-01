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
use serde::Deserialize;
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
/// The pinned dependency's live state transitions remain useful, but its
/// restore routine predates strict validation of proposal identifiers and
/// statuses. Keeping the inner type private ensures every restore reached
/// through `jterm_core::agent` first crosses [`validate_snapshot`].
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

#[derive(Deserialize)]
struct SnapshotInspection {
    transcript: Vec<Turn>,
    state: AgentState,
    next_proposal_id: u64,
}

fn validate_snapshot(snapshot: &AgentSessionSnapshot) -> Result<(), AgentSnapshotError> {
    // AgentSessionSnapshot's fields are deliberately private upstream. Its
    // own bounded serializer gives this compatibility layer an exact,
    // canonical view without exposing or duplicating a second public snapshot
    // format.
    let encoded = snapshot.to_json()?;
    let inspection: SnapshotInspection = serde_json::from_str(&encoded)
        .map_err(|error| AgentSnapshotError::Decode(error.to_string()))?;

    if inspection.transcript.len() > MAX_STORED_TRANSCRIPT_ENTRIES {
        return Err(AgentSnapshotError::Invalid(
            "transcript exceeds its entry limit",
        ));
    }
    let transcript_bytes = serde_json::to_vec(&inspection.transcript)
        .map_err(|error| AgentSnapshotError::Encode(error.to_string()))?
        .len();
    if transcript_bytes > MAX_STORED_TRANSCRIPT_BYTES {
        return Err(AgentSnapshotError::Invalid(
            "transcript exceeds its byte limit",
        ));
    }

    let mut highest_proposal_id = 0_u64;
    let mut proposal_statuses = HashMap::new();
    let mut pending_proposal = None;
    let mut observed_proposals = HashSet::new();
    for turn in &inspection.transcript {
        match turn {
            Turn::User(message) | Turn::AssistantSay(message) => {
                validate_snapshot_text(message, MAX_MESSAGE_BYTES, true)?;
            }
            Turn::AssistantThought(thought) => {
                validate_snapshot_text(thought, MAX_THOUGHT_BYTES, true)?;
            }
            Turn::AssistantProposed {
                id,
                command,
                status,
            } => {
                let id_value = id.get();
                if id_value == 0 || id_value <= highest_proposal_id {
                    return Err(AgentSnapshotError::Invalid(
                        "proposal ids are zero, duplicated, or out of order",
                    ));
                }
                validate_snapshot_command(command)?;
                highest_proposal_id = id_value;
                proposal_statuses.insert(id_value, *status);
                if *status == ProposalStatus::Pending && pending_proposal.replace(*id).is_some() {
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
                if proposal_statuses.get(&proposal_id.get()) != Some(&ProposalStatus::Approved)
                    || !observed_proposals.insert(proposal_id.get())
                {
                    return Err(AgentSnapshotError::Invalid(
                        "observation does not identify one approved proposal",
                    ));
                }
            }
            Turn::ProtocolError(message) => {
                validate_snapshot_text(message, MAX_MESSAGE_BYTES, false)?;
            }
        }
    }

    if inspection.next_proposal_id == 0
        || inspection.next_proposal_id == u64::MAX
        || inspection.next_proposal_id <= highest_proposal_id
    {
        return Err(AgentSnapshotError::Invalid(
            "next proposal id is stale or exhausted",
        ));
    }

    match inspection.state {
        AgentState::AwaitingApproval { proposal_id }
            if pending_proposal == Some(proposal_id)
                && proposal_statuses.get(&proposal_id.get()) == Some(&ProposalStatus::Pending) =>
        {
            Ok(())
        }
        AgentState::AwaitingApproval { .. } => Err(AgentSnapshotError::Invalid(
            "approval state does not identify the sole pending proposal",
        )),
        AgentState::AwaitingObservation { proposal_id }
            if pending_proposal.is_none()
                && proposal_statuses.get(&proposal_id.get()) == Some(&ProposalStatus::Approved)
                && !observed_proposals.contains(&proposal_id.get()) =>
        {
            Ok(())
        }
        AgentState::AwaitingObservation { .. } => Err(AgentSnapshotError::Invalid(
            "observation state does not identify one unobserved approved proposal",
        )),
        _ if pending_proposal.is_some() => Err(AgentSnapshotError::Invalid(
            "pending proposal exists outside approval state",
        )),
        _ => Ok(()),
    }
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
