//! Bounded task facts for every frontend. This is a read-only projection;
//! neither displayed readiness nor historical text grants execution authority.

use super::{AgentTask, TaskRuntimeKind, TaskStatus, TaskValidationStatus, TASK_GOAL_MAX_BYTES};
use crate::review_input::{safe_inline_display, safe_multiline_display};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskInspectorField {
    pub label: &'static str,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskInspector {
    pub fields: Vec<TaskInspectorField>,
    pub goal: Option<String>,
}

impl TaskInspector {
    pub fn from_task(task: &AgentTask) -> Self {
        let mut fields = Vec::new();
        let mut add = |label: &'static str, value: &str| {
            fields.push(TaskInspectorField {
                label,
                value: safe_inline_display(value, 4096),
            });
        };
        add("Task", &task.id.to_string());
        add("Provider", task.provider.display_name());
        add("State", task.status.label());
        add(
            "Origin",
            if task.brief.is_some() {
                "User-authored project task"
            } else if task.source_context.is_some() {
                "Captured terminal command"
            } else {
                "Recovered workspace; task instructions must be re-entered"
            },
        );
        add("Repository", &task.repo_root.to_string_lossy());
        add("Isolated worktree", &task.worktree_path.to_string_lossy());
        add("Branch", &task.branch);
        add("Review baseline", &task.base_commit);
        add("Runtime", task.runtime_kind.label());
        add("Created (UTC)", &utc_timestamp(task.created_at_ms));
        add("Updated (UTC)", &utc_timestamp(task.updated_at_ms));
        if let Some(source) = &task.source {
            add("Source session", &source.session_id);
            add("Source execution", &source.execution_id);
        }
        if let Some(command) = task.validation_command() {
            add("Validation command", command);
        }
        if let Some(shell) = task.validation_shell() {
            add("Validation shell", shell);
        }
        add("Validation", task.validation.status.label());
        if task.validation.attempt > 0 {
            add("Validation attempt", &task.validation.attempt.to_string());
        }
        if let Some(code) = task.validation.exit_code {
            add("Validation exit", &code.to_string());
        }
        if let Some(detail) = &task.validation.status_detail {
            add("Validation detail", detail);
        }
        if let Some(code) = task.exit_code {
            add("Agent exit", &code.to_string());
        }
        if let Some(detail) = &task.status_detail {
            add("Latest status detail", detail);
        }
        let guidance = if task.validation.status == TaskValidationStatus::Running {
            "Validation is running. Inspect its terminal before accepting this task."
        } else if task.status == TaskStatus::ReadyForReview {
            if task.requires_manual_review() {
                "Inspect the diff and result, finish any live Agent session, then explicitly accept the reviewed work. No validation command is configured."
            } else if task.validation.status == TaskValidationStatus::Passed {
                "Inspect the diff and latest validation result, finish any live Agent session, then accept the task."
            } else {
                "Inspect the diff, finish any live Agent session, then run the configured validation command."
            }
        } else if task.status == TaskStatus::Created {
            if task.brief.is_none() && task.source_context.is_none() {
                "This recovered workspace has no retained task instructions. Create a new task with an explicit goal before starting an Agent."
            } else {
                "The isolated task is ready. Starting its Agent requires a separate explicit action."
            }
        } else if task.status == TaskStatus::WaitingForApproval {
            "Review the exact pending request and use its available response actions. Navigation never approves a request."
        } else if task.status == TaskStatus::WaitingForHuman {
            "Read the latest Agent question and continue through an available reply or terminal action."
        } else if task.status.is_running() {
            "Inspect live output and requests. Stopping the Agent does not accept its work."
        } else if task.status == TaskStatus::Completed {
            "This task was explicitly accepted. Its worktree remains available for review."
        } else if task.runtime_kind == TaskRuntimeKind::Unassigned {
            "No Agent runtime is currently assigned to this task."
        } else {
            "Inspect the latest status and worktree before choosing another action."
        };
        add("Next step", guidance);
        Self {
            fields,
            goal: task
                .brief
                .as_ref()
                .map(|brief| safe_multiline_display(&brief.goal, TASK_GOAL_MAX_BYTES)),
        }
    }
}

fn utc_timestamp(milliseconds: u64) -> String {
    #[cfg(unix)]
    {
        let Ok(seconds) = libc::time_t::try_from(milliseconds / 1000) else {
            return "Unavailable".into();
        };
        let mut value = std::mem::MaybeUninit::<libc::tm>::uninit();
        // SAFETY: both pointers refer to live, correctly aligned storage;
        // gmtime_r initializes the output on success and does not share it.
        if unsafe { libc::gmtime_r(&seconds, value.as_mut_ptr()) }.is_null() {
            return "Unavailable".into();
        }
        let value = unsafe { value.assume_init() };
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            i64::from(value.tm_year) + 1900,
            value.tm_mon + 1,
            value.tm_mday,
            value.tm_hour,
            value.tm_min,
            value.tm_sec
        )
    }
    #[cfg(not(unix))]
    {
        let _ = milliseconds;
        "Unavailable".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn timestamp_has_explicit_stable_utc_meaning() {
        assert_eq!(utc_timestamp(0), "1970-01-01 00:00:00");
        assert_eq!(utc_timestamp(1_609_459_200_000), "2021-01-01 00:00:00");
    }
}
