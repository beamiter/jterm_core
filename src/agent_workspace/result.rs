//! A result view ties task identity, baseline, current changes and validation.
use super::{ConversationId, NativeSnapshot};
use crate::agent_task::worktree::{capture_task_review, WorktreeReviewSnapshot};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceValidationEvidence {
    pub fingerprint: String,
    pub head: String,
    pub base_commit: String,
    pub command: String,
    pub cwd: String,
    pub shell: String,
    pub started_at_ms: u64,
    pub completed_at_ms: Option<u64>,
    pub unchanged_after_run: bool,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceResultReport {
    pub conversation: ConversationId,
    pub task_id: crate::agent_task::TaskId,
    pub run_id: String,
    pub source_sequence: u64,
    pub baseline: Option<WorktreeReviewSnapshot>,
    pub current: Option<WorktreeReviewSnapshot>,
    pub validation: Option<WorkspaceValidationEvidence>,
    pub validation_current: bool,
    pub validation_status: String,
    pub validation_output: String,
    pub validation_output_truncated: bool,
    pub error: Option<String>,
}

impl WorkspaceResultReport {
    /// Read-only worker entrypoint. It neither runs tests nor accepts a task.
    pub fn inspect(snapshot: &NativeSnapshot) -> Self {
        Self::inspect_with_current(snapshot, capture_task_review(&snapshot.task))
    }

    pub(super) fn inspect_with_current(
        snapshot: &NativeSnapshot,
        captured: Result<WorktreeReviewSnapshot, String>,
    ) -> Self {
        let (current, error) = match captured {
            Ok(current) => (Some(current), None),
            Err(error) => (None, Some(error)),
        };
        let validation = snapshot
            .validation
            .as_ref()
            .and_then(|value| value.evidence.clone());
        let validation_current =
            validation
                .as_ref()
                .zip(current.as_ref())
                .is_some_and(|(evidence, current)| {
                    evidence.unchanged_after_run
                        && evidence.fingerprint == current.fingerprint
                        && evidence.command == snapshot.task.validation_command().unwrap_or("")
                        && evidence.shell == snapshot.task.validation_shell().unwrap_or("")
                        && !snapshot.ownership_locked
                });
        let validation_status = if snapshot.task.validation.status
            == crate::agent_task::TaskValidationStatus::Passed
            && !validation_current
        {
            "Stale or unbound evidence — run validation again".into()
        } else {
            snapshot.task.validation.status.label().into()
        };
        Self {
            conversation: snapshot.id.clone(),
            task_id: snapshot.task.id,
            run_id: snapshot.incarnation.clone(),
            source_sequence: snapshot.sequence,
            baseline: snapshot.review_baseline.clone(),
            current,
            validation,
            validation_current,
            validation_status,
            validation_output: snapshot
                .validation
                .as_ref()
                .map(|value| value.output.clone())
                .unwrap_or_default(),
            validation_output_truncated: snapshot
                .validation
                .as_ref()
                .is_some_and(|value| value.output_truncated),
            error,
        }
    }
}
