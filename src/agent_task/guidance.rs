//! Shared, bounded explanations for the next human step. This projection is
//! intentionally not an action list: live runtime gates remain authoritative.

use super::{AgentTask, DashboardActivity, TaskStatus, TaskValidationStatus};
use crate::review_input::safe_multiline_display;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskGuidance {
    pub title: &'static str,
    pub detail: &'static str,
    /// Current failure/question detail, bounded and safe for display.
    pub reason: Option<String>,
}

impl TaskGuidance {
    pub fn from_task(task: &AgentTask, activity: DashboardActivity) -> Self {
        let (title, detail) = if task.status == TaskStatus::Archived {
            (
                "Task hidden",
                "The worktree and recorded task evidence remain available.",
            )
        } else if task.status == TaskStatus::WaitingForApproval {
            (
                "Review the pending request",
                "Read the exact command or file changes and their reason, then choose from the available response actions for this request.",
            )
        } else if task.status == TaskStatus::WaitingForHuman {
            (
                "Your reply is needed",
                "Read the Agent's latest question and continue using an available reply or terminal action. Its work remains unaccepted.",
            )
        } else if task.validation.status == TaskValidationStatus::Running {
            (
                "Validation is running",
                "Open the validation output to follow this attempt. Wait for its result before accepting the task.",
            )
        } else if activity.native_preparing {
            (
                "Preparing the Agent",
                "The task is starting. Its output and requests will appear here; the work has not been accepted.",
            )
        } else {
            match task.status {
                TaskStatus::Created if task.brief.is_none() && task.source_context.is_none() => (
                    "Task instructions are missing",
                    "This recovered worktree has no retained instructions. Inspect its existing work and create a task with an explicit goal before starting an Agent.",
                ),
                TaskStatus::Created => (
                    "Ready to start",
                    "Review the goal, provider and isolated worktree, then choose an available Start action. Creating a task does not start its Agent.",
                ),
                TaskStatus::Starting | TaskStatus::Working => (
                    "Agent is working",
                    "Follow its output and requests. You can stop the run when needed; stopping does not accept the work.",
                ),
                TaskStatus::ReadyForReview if activity.has_active_agent_stream => (
                    "Review or continue the live session",
                    "Inspect the changes and send follow-up feedback if needed. Finish the Agent session before running validation or accepting the task.",
                ),
                TaskStatus::ReadyForReview if task.validation.status.needs_attention() => (
                    "Resolve the validation result",
                    "Inspect the latest validation output and the worktree changes. Correct any issue before retrying validation; an unsuccessful or interrupted check is not a pass.",
                ),
                TaskStatus::ReadyForReview if task.requires_manual_review() => (
                    "Review and accept explicitly",
                    "No validation command is configured. Inspect the changes and result, then use manual acceptance when satisfied.",
                ),
                TaskStatus::ReadyForReview if task.validation.status == TaskValidationStatus::Passed => (
                    "Validation passed; review the changes",
                    "Check the diff and latest validation output, then explicitly accept the task. A passing check never accepts the work automatically.",
                ),
                TaskStatus::ReadyForReview => (
                    "Run the configured validation",
                    "Inspect the changes, then run the recorded validation command in this task's worktree. Review its result before accepting.",
                ),
                TaskStatus::Failed => (
                    "Inspect the failed run",
                    "Read the failure details and inspect the preserved worktree before choosing an available recovery action. The existing changes have not been accepted.",
                ),
                TaskStatus::Cancelled => (
                    "Inspect the cancelled run",
                    "The run was stopped. Review its output and preserved worktree before continuing; cancellation does not accept or discard the changes.",
                ),
                TaskStatus::Completed => (
                    "Task accepted",
                    "The task has been explicitly accepted. Its changes, validation record and worktree remain available for inspection.",
                ),
                // Handled above; retain an exhaustive match as statuses evolve.
                TaskStatus::WaitingForApproval | TaskStatus::WaitingForHuman | TaskStatus::Archived => unreachable!(),
            }
        };
        Self {
            title,
            detail,
            reason: Self::reason_for(
                task.status,
                task.validation.status,
                task.status_detail.as_deref(),
                task.validation.status_detail.as_deref(),
            ),
        }
    }

    /// Current explanation for row adapters that hold task facts rather than
    /// an `AgentTask`. The inspector and compact rows use the same priority.
    pub fn reason_for(
        status: TaskStatus,
        validation_status: TaskValidationStatus,
        status_detail: Option<&str>,
        validation_detail: Option<&str>,
    ) -> Option<String> {
        if matches!(
            status,
            TaskStatus::WaitingForApproval | TaskStatus::WaitingForHuman | TaskStatus::Failed
        ) {
            status_detail
        } else if validation_status.needs_attention() && status != TaskStatus::Archived {
            validation_detail
        } else if status == TaskStatus::Cancelled {
            status_detail
        } else {
            None
        }
        .filter(|detail| !detail.trim().is_empty())
        .map(|detail| safe_multiline_display(detail.trim(), 2048))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::{
        AgentProvider, NewTask, TaskBrief, TaskManager, TaskRuntimeKind, TaskValidationRecipe,
    };

    fn task() -> AgentTask {
        let mut manager = TaskManager::new();
        let id = manager
            .create(NewTask {
                title: "Task command center".into(),
                provider: AgentProvider::Codex,
                repo_root: "/repo".into(),
                worktree_path: "/worktree".into(),
                branch: "task/command-center".into(),
                base_commit: "a".repeat(40),
                source_context: None,
            })
            .unwrap();
        let mut task = manager.get(id).unwrap().clone();
        task.brief = Some(TaskBrief::new("Improve task management", None).unwrap());
        task
    }

    #[test]
    fn review_guidance_distinguishes_retained_sessions_checks_and_manual_acceptance() {
        let mut task = task();
        task.status = TaskStatus::ReadyForReview;
        task.runtime_kind = TaskRuntimeKind::Native;
        let idle = DashboardActivity::default();
        let live = DashboardActivity {
            has_active_agent_stream: true,
            ..idle
        };
        assert_eq!(
            TaskGuidance::from_task(&task, live).title,
            "Review or continue the live session"
        );
        assert_eq!(
            TaskGuidance::from_task(&task, idle).title,
            "Review and accept explicitly"
        );
        task.brief.as_mut().unwrap().validation =
            Some(TaskValidationRecipe::new("cargo test", "/bin/sh").unwrap());
        assert_eq!(
            TaskGuidance::from_task(&task, idle).title,
            "Run the configured validation"
        );
        task.validation.status = TaskValidationStatus::Passed;
        assert_eq!(
            TaskGuidance::from_task(&task, idle).title,
            "Validation passed; review the changes"
        );
        assert_eq!(
            TaskGuidance::from_task(&task, live).title,
            "Review or continue the live session"
        );
        task.validation.status = TaskValidationStatus::Running;
        assert_eq!(
            TaskGuidance::from_task(&task, idle).title,
            "Validation is running"
        );
        for result in [
            TaskValidationStatus::Failed,
            TaskValidationStatus::Inconclusive,
            TaskValidationStatus::Cancelled,
        ] {
            task.validation.status = result;
            assert_eq!(
                TaskGuidance::from_task(&task, idle).title,
                "Resolve the validation result"
            );
        }
    }

    #[test]
    fn approval_and_reply_are_not_obscured_by_preparation_or_old_validation() {
        let mut task = task();
        task.validation.status = TaskValidationStatus::Failed;
        task.validation.status_detail = Some("Old validation failure".into());
        task.status_detail = Some("Current request".into());
        let activity = DashboardActivity {
            native_preparing: true,
            ..Default::default()
        };
        task.status = TaskStatus::WaitingForApproval;
        let guidance = TaskGuidance::from_task(&task, activity);
        assert_eq!(guidance.title, "Review the pending request");
        assert_eq!(guidance.reason.as_deref(), Some("Current request"));
        task.status = TaskStatus::WaitingForHuman;
        assert_eq!(
            TaskGuidance::from_task(&task, activity).title,
            "Your reply is needed"
        );
    }

    #[test]
    fn guidance_is_bounded_and_cannot_mutate_or_complete_a_task() {
        let mut task = task();
        task.status = TaskStatus::Failed;
        task.status_detail = Some(format!("\u{1b}[2J\u{202e}{}", "错误\n".repeat(3000)));
        let before = task.clone();
        let guidance = TaskGuidance::from_task(&task, DashboardActivity::default());
        let reason = guidance.reason.unwrap();
        assert!(reason.len() <= 2048);
        assert!(!reason.contains(['\u{1b}', '\u{202e}']));
        assert_eq!(task, before);
        task.status = TaskStatus::Archived;
        assert!(TaskGuidance::from_task(&task, DashboardActivity::default())
            .reason
            .is_none());
        task.status = TaskStatus::Created;
        task.brief = None;
        assert_eq!(
            TaskGuidance::from_task(&task, DashboardActivity::default()).title,
            "Task instructions are missing"
        );
    }
}
