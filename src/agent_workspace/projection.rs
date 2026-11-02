//! Shared, read-only presentation of workspace facts. Neither catalog data nor
//! a suggested primary action grants control, permission or task acceptance.

use super::{ApiSnapshot, ApiState, ConversationKind, NativeSnapshot, RuntimeState, WorkspaceRow};
use crate::agent_sessions::SessionStatus;
use crate::agent_task::{
    AgentProvider, AgentTask, DashboardActivity, DashboardAttentionKind, DashboardRow, TaskId,
    TaskRuntimeKind, TaskStatus, TaskValidationStatus,
};
use serde::{Deserialize, Serialize};

/// One navigation destination per row. Historical CLI input requests never
/// become live approvals or contribute to these counts.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceAttention {
    Approval,
    Reply,
    Validation,
    Review,
    Failed,
    Interrupted,
    Conflict,
}

impl WorkspaceAttention {
    pub const ALL: [Self; 7] = [
        Self::Approval,
        Self::Reply,
        Self::Validation,
        Self::Review,
        Self::Failed,
        Self::Interrupted,
        Self::Conflict,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Approval => "Permission requested",
            Self::Reply => "Needs reply",
            Self::Validation => "Validation needs review",
            Self::Review => "Ready to review",
            Self::Failed => "Execution failed",
            Self::Interrupted => "Result unconfirmed",
            Self::Conflict => "Owner conflict",
        }
    }

    pub fn query_value(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Reply => "input",
            Self::Validation => "validation",
            Self::Review => "review",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Conflict => "conflict",
        }
    }

    pub(crate) fn from_dashboard(value: DashboardAttentionKind) -> Self {
        match value {
            DashboardAttentionKind::Approval => Self::Approval,
            DashboardAttentionKind::Input => Self::Reply,
            DashboardAttentionKind::Validation => Self::Validation,
            DashboardAttentionKind::Review => Self::Review,
            DashboardAttentionKind::Failed => Self::Failed,
        }
    }
}

/// Adapter affordances, not permission or current availability. A frontend must
/// still obtain a fresh controller, recheck the exact snapshot and let the
/// runtime validate every operation. Missing facts from an older host are
/// deliberately unknown rather than inferred from a provider's display name.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceCapabilities {
    pub known: bool,
    pub terminal: bool,
    pub follow_up: bool,
    pub native_resume: bool,
    pub terminal_handoff: bool,
    pub structured_approval: bool,
    pub history_paging: bool,
    pub background_execution: bool,
}

impl WorkspaceCapabilities {
    pub fn cli() -> Self {
        Self {
            known: true,
            terminal: true,
            background_execution: true,
            ..Self::default()
        }
    }

    pub fn api() -> Self {
        Self {
            known: true,
            follow_up: true,
            history_paging: true,
            background_execution: true,
            ..Self::default()
        }
    }

    pub fn native(provider: AgentProvider) -> Self {
        match provider {
            AgentProvider::Codex => Self {
                known: true,
                follow_up: true,
                native_resume: true,
                terminal_handoff: true,
                structured_approval: true,
                history_paging: true,
                background_execution: true,
                ..Self::default()
            },
            AgentProvider::Claude => Self {
                known: true,
                follow_up: true,
                terminal_handoff: true,
                background_execution: true,
                ..Self::default()
            },
            AgentProvider::Kimi => Self {
                known: true,
                background_execution: true,
                ..Self::default()
            },
            AgentProvider::OpenCode => Self::default(),
        }
    }
}

/// Optional facts enrich the stable row without making old database records
/// incompatible. Runtime, task, validation and recorded history stay separate.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceFacts {
    pub branch: Option<String>,
    pub worktree: Option<String>,
    pub recorded_status: Option<SessionStatus>,
    pub task_id: Option<TaskId>,
    pub task_status: Option<TaskStatus>,
    /// Explicit accepted lifecycle, retained when that task is later archived.
    pub task_accepted: bool,
    pub task_runtime: Option<TaskRuntimeKind>,
    pub validation_status: Option<TaskValidationStatus>,
    pub api_state: Option<ApiState>,
    pub attention: Option<WorkspaceAttention>,
    pub status_detail: Option<String>,
    pub interrupted: bool,
    pub preparing: bool,
    pub agent_stream: bool,
    pub agent_terminal: bool,
    pub validation_terminal: bool,
    pub source_sequence: Option<u64>,
    /// An exact live approval destination for navigation only. Historical
    /// snapshots and stopped providers must not retain one.
    pub approval_id: Option<String>,
    pub capabilities: WorkspaceCapabilities,
}

impl WorkspaceFacts {
    /// A scanner may report source metadata, never current runtime authority.
    /// Apply this at IndexSessions admission before publishing GUI-supplied rows.
    pub fn recorded_only(self) -> Self {
        Self {
            branch: self.branch,
            recorded_status: self.recorded_status,
            ..Self::default()
        }
    }

    /// Reuse the dashboard's current classification rather than inventing a
    /// competing set of task/validation precedence rules in each frontend.
    pub fn from_task(task: &AgentTask, activity: DashboardActivity) -> Self {
        let dashboard = DashboardRow::from_task(task, activity);
        Self {
            branch: Some(task.branch.clone()),
            worktree: Some(task.worktree_path.to_string_lossy().into_owned()),
            task_id: Some(task.id),
            task_status: Some(task.status),
            task_accepted: task.status == TaskStatus::Completed
                || (task.status == TaskStatus::Archived
                    && task.archived_from == Some(TaskStatus::Completed)),
            task_runtime: Some(task.runtime_kind),
            validation_status: Some(task.validation.status),
            attention: dashboard
                .attention_kind()
                .map(WorkspaceAttention::from_dashboard),
            status_detail: dashboard.status_detail,
            preparing: activity.native_preparing,
            agent_stream: activity.has_active_agent_stream,
            agent_terminal: activity.has_agent_terminal,
            validation_terminal: activity.has_validation_terminal,
            // A local task may not yet be hosted or preflighted. Its lifecycle
            // alone cannot attest a transport or a provider protocol version.
            ..Self::default()
        }
    }

    pub fn from_native(snapshot: &NativeSnapshot) -> Self {
        let activity = DashboardActivity {
            has_agent_terminal: snapshot.terminal.is_some() && snapshot.ownership_locked,
            has_validation_terminal: snapshot
                .validation
                .as_ref()
                .is_some_and(|validation| validation.terminal.is_some()),
            has_active_agent_stream: snapshot.live,
            native_preparing: snapshot.preparing,
        };
        let mut facts = Self::from_task(&snapshot.task, activity);
        facts.source_sequence = Some(snapshot.sequence);
        facts.interrupted = snapshot.interrupted;
        facts.capabilities = WorkspaceCapabilities::native(snapshot.task.provider);
        if snapshot.interrupted {
            facts.attention = Some(WorkspaceAttention::Interrupted);
        } else if snapshot.live {
            facts.approval_id = snapshot
                .view
                .as_ref()
                .and_then(|view| view.pending_approvals.first())
                .map(|approval| approval.id.to_string());
        } else if matches!(
            facts.attention,
            Some(WorkspaceAttention::Approval | WorkspaceAttention::Reply)
        ) {
            // Saved requests are historical evidence. Opening them must not
            // revive a provider request or a notification that can be acted on.
            facts.attention = None;
        }
        facts
    }

    pub fn from_api(snapshot: &ApiSnapshot) -> Self {
        Self {
            api_state: Some(snapshot.state),
            interrupted: snapshot.state == ApiState::Interrupted,
            preparing: snapshot.state == ApiState::Starting,
            source_sequence: Some(snapshot.sequence),
            attention: match snapshot.state {
                ApiState::Interrupted => Some(WorkspaceAttention::Interrupted),
                ApiState::Failed => Some(WorkspaceAttention::Failed),
                _ => None,
            },
            status_detail: snapshot
                .error
                .as_deref()
                .map(|text| crate::review_input::safe_inline_display(text, 1024)),
            capabilities: WorkspaceCapabilities::api(),
            ..Self::default()
        }
    }
}

/// Primary actions only navigate to a conversation or its evidence. They do
/// not approve, send, resume, retry, stop, accept or steal a control lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspacePrimaryAction {
    OpenConversation,
    OpenTerminal,
    ReviewRequest,
    Reply,
    ViewValidation,
    ReviewResult,
    ViewError,
    CheckOutcome,
    ViewOwner,
}

impl WorkspacePrimaryAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenConversation => "Open conversation",
            Self::OpenTerminal => "Open terminal",
            Self::ReviewRequest => "Review request",
            Self::Reply => "Open reply",
            Self::ViewValidation => "View validation",
            Self::ReviewResult => "Review result",
            Self::ViewError => "View error",
            Self::CheckOutcome => "Check outcome",
            Self::ViewOwner => "View current owner",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspacePresentation {
    pub status_label: String,
    pub detail: Option<String>,
    pub runtime_label: &'static str,
    pub task_label: Option<&'static str>,
    pub validation_label: Option<&'static str>,
    pub attention: Option<WorkspaceAttention>,
    pub primary_action: WorkspacePrimaryAction,
    pub capabilities: WorkspaceCapabilities,
}

impl WorkspaceRow {
    pub fn attention(&self) -> Option<WorkspaceAttention> {
        if self.archived {
            None
        } else if self.facts.interrupted {
            Some(WorkspaceAttention::Interrupted)
        } else if self.runtime == RuntimeState::Conflict {
            Some(WorkspaceAttention::Conflict)
        } else {
            self.facts.attention
        }
    }

    pub fn presentation(&self) -> WorkspacePresentation {
        let attention = self.attention();
        let primary_action = match attention {
            Some(WorkspaceAttention::Approval) => WorkspacePrimaryAction::ReviewRequest,
            Some(WorkspaceAttention::Reply) => WorkspacePrimaryAction::Reply,
            Some(WorkspaceAttention::Validation) => WorkspacePrimaryAction::ViewValidation,
            Some(WorkspaceAttention::Review) => WorkspacePrimaryAction::ReviewResult,
            Some(WorkspaceAttention::Failed) => WorkspacePrimaryAction::ViewError,
            Some(WorkspaceAttention::Interrupted) => WorkspacePrimaryAction::CheckOutcome,
            Some(WorkspaceAttention::Conflict) => WorkspacePrimaryAction::ViewOwner,
            None if self.kind == ConversationKind::Cli && !self.archived => {
                WorkspacePrimaryAction::OpenTerminal
            }
            None => WorkspacePrimaryAction::OpenConversation,
        };
        let status = if self.archived {
            "Archived"
        } else if let Some(attention) = attention {
            attention.label()
        } else if self.facts.validation_status == Some(TaskValidationStatus::Running) {
            "Validation running"
        } else if self.facts.preparing {
            "Preparing"
        } else if let Some(task_status) = self.facts.task_status {
            // A successful validation never turns ReadyForReview into Completed.
            task_status.label()
        } else if let Some(api_state) = self.facts.api_state {
            match api_state {
                ApiState::Starting => "Preparing request",
                ApiState::Running => "Reply in progress",
                ApiState::Stopping => "Stopping request",
                ApiState::Completed => "Reply finished",
                ApiState::Cancelled => "Request stopped",
                ApiState::Failed => "Request failed",
                ApiState::Interrupted => "Result unconfirmed",
            }
        } else if self.runtime == RuntimeState::Idle && self.kind == ConversationKind::Api {
            "Ready"
        } else if self.runtime == RuntimeState::Idle && self.kind == ConversationKind::Task {
            // An indexed task may have no current task-store or host facts.
            // Idle observation alone cannot promise that it can be resumed.
            "Task details unavailable"
        } else {
            self.runtime.label()
        };
        let mut detail = self.facts.status_detail.clone();
        if detail.is_none() && self.kind == ConversationKind::Cli {
            detail = self.facts.recorded_status.map(|status| {
                format!(
                    "Recorded history: {}",
                    match status {
                        SessionStatus::Working => "work activity",
                        SessionStatus::Waiting => "input requested",
                        SessionStatus::Idle => "turn finished",
                        SessionStatus::Unknown => "state unavailable",
                        SessionStatus::Archived => "provider archived",
                    }
                )
            });
        }
        WorkspacePresentation {
            status_label: status.into(),
            detail,
            runtime_label: if self.kind == ConversationKind::Task
                && self.runtime == RuntimeState::Idle
            {
                "Idle"
            } else {
                self.runtime.label()
            },
            task_label: self.facts.task_status.map(TaskStatus::label),
            validation_label: self
                .facts
                .validation_status
                .map(TaskValidationStatus::label),
            attention,
            primary_action,
            capabilities: self.facts.capabilities,
        }
    }

    /// For local tasks that have not yet been published by the host. Namespace
    /// must be the same durable application/task-store namespace used at launch.
    pub fn from_task(
        namespace: &str,
        task: &AgentTask,
        activity: DashboardActivity,
    ) -> Result<Self, String> {
        let dashboard = DashboardRow::from_task(task, activity);
        // Lifecycle text can outlive its process. Only resolved owners are
        // evidence that this fallback row has a currently live runtime.
        let running = activity.has_agent_terminal
            || activity.has_validation_terminal
            || activity.has_active_agent_stream
            || activity.native_preparing;
        Ok(Self {
            id: super::task_id(namespace, &task.id.to_string())?,
            kind: ConversationKind::Task,
            title: dashboard.title,
            provider: task.provider.display_name().into(),
            cwd: task.repo_root.to_string_lossy().into_owned(),
            preview: task.status.label().into(),
            updated_at_ms: task.updated_at_ms,
            archived: task.status == TaskStatus::Archived,
            pinned: false,
            tags: Vec::new(),
            runtime: if running {
                RuntimeState::Running
            } else {
                RuntimeState::Idle
            },
            facts: WorkspaceFacts::from_task(task, activity),
        })
    }

    /// Apply only an exact conversation's authoritative API facts. The caller
    /// still handles active admission receipts and retained owner conflicts.
    pub fn apply_api_snapshot(&mut self, snapshot: &ApiSnapshot) -> Result<(), String> {
        if self.kind != ConversationKind::Api || self.id != snapshot.id {
            return Err("API snapshot belongs to another workspace conversation".into());
        }
        self.facts = WorkspaceFacts::from_api(snapshot);
        self.runtime = match snapshot.state {
            ApiState::Starting => RuntimeState::Starting,
            ApiState::Running => RuntimeState::Running,
            ApiState::Stopping => RuntimeState::Stopping,
            ApiState::Completed | ApiState::Cancelled => RuntimeState::Idle,
            ApiState::Failed => RuntimeState::Failed,
            ApiState::Interrupted => RuntimeState::Conflict,
        };
        Ok(())
    }
}

/// Counts disjoint rows in an already searched scope; no row counts twice.
pub fn workspace_attention_counts(rows: &[WorkspaceRow]) -> Vec<(WorkspaceAttention, usize)> {
    WorkspaceAttention::ALL
        .into_iter()
        .map(|kind| {
            (
                kind,
                rows.iter()
                    .filter(|row| row.attention() == Some(kind))
                    .count(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_workspace::ConversationId;

    fn row(kind: ConversationKind) -> WorkspaceRow {
        WorkspaceRow {
            id: ConversationId("test:one".into()),
            kind,
            title: "Fixture".into(),
            provider: "Codex".into(),
            cwd: "/work/repo".into(),
            preview: String::new(),
            updated_at_ms: 1,
            archived: false,
            pinned: false,
            tags: Vec::new(),
            runtime: RuntimeState::Idle,
            facts: WorkspaceFacts::default(),
        }
    }

    #[test]
    fn old_rows_and_recorded_cli_requests_cannot_advertise_current_approval() {
        let old = r#"{"id":"test:one","kind":"Cli","title":"Old","provider":"Codex","cwd":"/work/repo","preview":"approve?","updated_at_ms":1,"archived":false,"pinned":false,"tags":[],"runtime":"Idle"}"#;
        let mut value: WorkspaceRow = serde_json::from_str(old).unwrap();
        assert!(!value.presentation().capabilities.known);
        value.facts.recorded_status = Some(SessionStatus::Waiting);
        assert_eq!(value.attention(), None);
        assert_eq!(
            value.presentation().primary_action,
            WorkspacePrimaryAction::OpenTerminal
        );
        assert!(value
            .presentation()
            .detail
            .unwrap()
            .contains("Recorded history"));
    }

    #[test]
    fn external_task_without_current_facts_does_not_claim_resume_readiness() {
        let old = r#"{"id":"test:one","kind":"Task","title":"External task","provider":"Codex","cwd":"/work/repo","preview":"Created","updated_at_ms":1,"archived":false,"pinned":false,"tags":[],"runtime":"Idle"}"#;
        let mut value: WorkspaceRow = serde_json::from_str(old).unwrap();
        value.facts.recorded_status = Some(SessionStatus::Waiting);
        let view = value.presentation();
        assert_eq!(view.status_label, "Task details unavailable");
        assert_eq!(view.runtime_label, "Idle");
        assert_eq!(view.task_label, None);
        assert_eq!(view.attention, None);
        assert_eq!(
            view.primary_action,
            WorkspacePrimaryAction::OpenConversation
        );
        assert_eq!(view.capabilities, WorkspaceCapabilities::default());

        // Current observed execution and stronger lifecycle evidence retain
        // their existing precedence; this fallback does not change authority.
        value.runtime = RuntimeState::Running;
        assert_eq!(
            value.presentation().status_label,
            RuntimeState::Running.label()
        );
        value.runtime = RuntimeState::Conflict;
        assert_eq!(
            value.presentation().primary_action,
            WorkspacePrimaryAction::ViewOwner
        );
        value.runtime = RuntimeState::Idle;
        value.facts.task_status = Some(TaskStatus::Created);
        assert_eq!(
            value.presentation().status_label,
            TaskStatus::Created.label()
        );
        value.archived = true;
        assert_eq!(value.presentation().status_label, "Archived");
    }

    #[test]
    fn validation_pass_and_task_acceptance_remain_different_facts() {
        let mut value = row(ConversationKind::Task);
        value.facts.task_status = Some(TaskStatus::ReadyForReview);
        value.facts.validation_status = Some(TaskValidationStatus::Passed);
        value.facts.attention = Some(WorkspaceAttention::Review);
        let view = value.presentation();
        assert_eq!(view.primary_action, WorkspacePrimaryAction::ReviewResult);
        assert_eq!(view.task_label, Some(TaskStatus::ReadyForReview.label()));
        assert_eq!(
            view.validation_label,
            Some(TaskValidationStatus::Passed.label())
        );
        value.archived = true;
        assert_eq!(value.attention(), None);
    }

    #[test]
    fn interrupted_results_override_old_attention_and_are_not_resume_actions() {
        let mut value = row(ConversationKind::Task);
        value.facts.attention = Some(WorkspaceAttention::Approval);
        value.facts.interrupted = true;
        value.runtime = RuntimeState::Conflict;
        assert_eq!(value.attention(), Some(WorkspaceAttention::Interrupted));
        assert_eq!(
            value.presentation().primary_action,
            WorkspacePrimaryAction::CheckOutcome
        );
    }

    #[test]
    fn provider_matrix_does_not_invent_claude_print_permissions_or_native_restore() {
        let codex = WorkspaceCapabilities::native(AgentProvider::Codex);
        let claude = WorkspaceCapabilities::native(AgentProvider::Claude);
        assert!(codex.native_resume && codex.structured_approval);
        assert!(claude.follow_up && claude.terminal_handoff);
        assert!(!claude.native_resume && !claude.structured_approval);
        assert!(!WorkspaceCapabilities::native(AgentProvider::Kimi).follow_up);
    }

    #[test]
    fn scanner_projection_cannot_publish_live_attention_or_capabilities() {
        let facts = WorkspaceFacts {
            branch: Some("feature/input".into()),
            recorded_status: Some(SessionStatus::Waiting),
            task_status: Some(TaskStatus::WaitingForApproval),
            attention: Some(WorkspaceAttention::Approval),
            interrupted: true,
            approval_id: Some("made-up-request".into()),
            capabilities: WorkspaceCapabilities::native(AgentProvider::Codex),
            ..WorkspaceFacts::default()
        }
        .recorded_only();
        assert_eq!(facts.branch.as_deref(), Some("feature/input"));
        assert_eq!(facts.recorded_status, Some(SessionStatus::Waiting));
        assert_eq!(facts.attention, None);
        assert_eq!(facts.task_status, None);
        assert_eq!(facts.approval_id, None);
        assert!(!facts.interrupted && !facts.capabilities.known);
    }

    #[test]
    fn api_updates_are_identity_bound_and_unknown_outcomes_stay_visible() {
        let mut value = row(ConversationKind::Api);
        let mut snapshot = ApiSnapshot {
            id: value.id.clone(),
            epoch: 1,
            sequence: 2,
            state: ApiState::Interrupted,
            user_text: String::new(),
            submitted_draft_revision: 0,
            preview: String::new(),
            result: None,
            error: Some("Host restarted".into()),
        };
        value.apply_api_snapshot(&snapshot).unwrap();
        assert_eq!(
            value.presentation().primary_action,
            WorkspacePrimaryAction::CheckOutcome
        );
        snapshot.id = ConversationId("test:other".into());
        assert!(value.apply_api_snapshot(&snapshot).is_err());
        assert!(value.facts.interrupted);
    }
}
