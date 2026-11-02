//! Structured native executions owned by the persistent session host.
use super::model::{ConversationId, WorkspaceRow};
use crate::agent_task::{
    AgentRuntimePollReport, AgentTask, ApprovalDecision, ApprovalId, CodexAppServerExitReport,
    CodexAppServerViewSnapshot, NativePromptPolicy, TaskId,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize)]
pub struct NativeLaunchContext {
    pub search_path: Option<String>,
    pub codex_home: PathBuf,
    pub claude_home: PathBuf,
    /// Only provider authentication overrides; never written to the database.
    pub(crate) environment: Vec<(String, String)>,
}
const AUTH_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "KIMI_API_KEY",
];
fn auth_for_provider(name: &str, provider: crate::agent_task::AgentProvider) -> bool {
    match provider {
        crate::agent_task::AgentProvider::Claude => {
            name.starts_with("ANTHROPIC_") || name == "CLAUDE_CODE_OAUTH_TOKEN"
        }
        crate::agent_task::AgentProvider::Kimi => name == "KIMI_API_KEY",
        _ => false,
    }
}
impl std::fmt::Debug for NativeLaunchContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeLaunchContext")
            .field("codex_home", &self.codex_home)
            .field("claude_home", &self.claude_home)
            .field("authentication_override_count", &self.environment.len())
            .finish_non_exhaustive()
    }
}
impl NativeLaunchContext {
    /// Resolve profile selectors in the requesting process, before crossing a
    /// host that might have been started by a different frontend or profile.
    pub fn capture(provider: crate::agent_task::AgentProvider) -> Result<Self, String> {
        let home = dirs::home_dir().ok_or("Cannot locate the native provider profile")?;
        let context = Self {
            search_path: std::env::var("PATH").ok(),
            codex_home: std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
            claude_home: std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude")),
            environment: AUTH_ENV
                .iter()
                .filter(|name| auth_for_provider(name, provider))
                .filter_map(|name| {
                    std::env::var(name)
                        .ok()
                        .map(|value| ((*name).into(), value))
                })
                .collect(),
        };
        context.validate()?;
        Ok(context)
    }
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !self.codex_home.is_absolute()
            || !self.claude_home.is_absolute()
            || self.codex_home.to_string_lossy().len() > 8192
            || self.claude_home.to_string_lossy().len() > 8192
            || self
                .search_path
                .as_ref()
                .is_some_and(|p| p.len() > 64 * 1024 || p.contains('\0'))
            || self.environment.len() > AUTH_ENV.len()
            || self.environment.iter().any(|(name, value)| {
                !AUTH_ENV.contains(&name.as_str())
                    || value.len() > 64 * 1024
                    || value.contains('\0')
            })
        {
            return Err("Invalid native provider profile or authentication context".into());
        }
        Ok(())
    }
    pub(crate) fn configure_authentication(
        &self,
        command: &mut std::process::Command,
        provider: crate::agent_task::AgentProvider,
    ) {
        for name in AUTH_ENV {
            command.env_remove(name);
        }
        for (name, value) in &self.environment {
            if auth_for_provider(name, provider) {
                command.env(name, value);
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct NativeLaunch {
    pub namespace: String,
    pub task: AgentTask,
    pub policy: NativePromptPolicy,
    pub context: NativeLaunchContext,
    pub view: String,
}
impl std::fmt::Debug for NativeLaunch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeLaunch")
            .field("namespace", &self.namespace)
            .field("task_id", &self.task.id)
            .field("provider", &self.task.provider)
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NativeControlLease {
    pub id: ConversationId,
    pub incarnation: String,
    pub generation: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub enum NativeAction {
    Cancel,
    Approval {
        id: ApprovalId,
        decision: ApprovalDecision,
    },
    Prompt {
        text: String,
        policy: NativePromptPolicy,
        #[serde(default)]
        receipt: Option<String>,
    },
    Finish,
    History {
        reset: bool,
    },
    Handoff {
        context: NativeLaunchContext,
    },
    Resume {
        context: NativeLaunchContext,
        policy: NativePromptPolicy,
    },
    Policy {
        policy: NativePromptPolicy,
    },
    Validate {
        expected_sequence: u64,
    },
    MutateTask {
        expected_sequence: u64,
        mutation: NativeTaskMutation,
    },
    UpdateTask {
        task: Box<AgentTask>,
    },
}
impl std::fmt::Debug for NativeAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Cancel => "Cancel",
            Self::Approval { .. } => "Approval",
            Self::Prompt { .. } => "Prompt",
            Self::Finish => "Finish",
            Self::History { .. } => "History",
            Self::Handoff { .. } => "Handoff",
            Self::Resume { .. } => "Resume",
            Self::Policy { .. } => "Policy",
            Self::UpdateTask { .. } => "UpdateTask",
            Self::Validate { .. } => "Validate",
            Self::MutateTask { .. } => "MutateTask",
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeReportEvent {
    pub sequence: u64,
    pub report: AgentRuntimePollReport,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeSnapshot {
    #[serde(default = "default_redaction")]
    pub redact_secrets: bool,
    #[serde(default)]
    pub validation_evidence_redacted: bool,
    pub id: ConversationId,
    pub namespace: String,
    pub incarnation: String,
    pub sequence: u64,
    pub task: AgentTask,
    pub view: Option<CodexAppServerViewSnapshot>,
    pub preparing: bool,
    pub live: bool,
    pub ownership_locked: bool,
    pub interrupted: bool,
    pub can_prompt: bool,
    pub can_resume: bool,
    #[serde(default)]
    pub can_start_validation: bool,
    #[serde(default)]
    pub manual_review: bool,
    pub can_continue_in_terminal: bool,
    pub exit: Option<CodexAppServerExitReport>,
    pub events: Vec<NativeReportEvent>,
    pub native_home: Option<PathBuf>,
    pub provider_home: PathBuf,
    pub source_profile: PathBuf,
    pub handoff_pending: bool,
    pub terminal: Option<crate::agent_sessions::AgentSession>,
    pub operations: Vec<String>,
    #[serde(default)]
    pub validation: Option<HostedValidationSnapshot>,
}
impl NativeSnapshot {
    pub fn task_id(&self) -> TaskId {
        self.task.id
    }
    pub(crate) fn row(&self) -> WorkspaceRow {
        use super::model::{ConversationKind, RuntimeState};
        use crate::agent_task::TaskStatus;
        WorkspaceRow {
            id: self.id.clone(),
            kind: ConversationKind::Task,
            title: self.task.title.clone(),
            provider: self.task.provider.display_name().into(),
            cwd: self.task.repo_root.to_string_lossy().into_owned(),
            preview: if self.interrupted {
                "Execution interrupted; inspect history before continuing".into()
            } else {
                format!("{} · {}", self.task.status.label(), self.task.branch)
            },
            updated_at_ms: self.task.updated_at_ms,
            archived: self.task.status == TaskStatus::Archived,
            pinned: false,
            tags: vec![],
            runtime: if self.terminal.is_some() && self.ownership_locked && !self.interrupted {
                RuntimeState::Running
            } else if self.interrupted
                || (self.ownership_locked
                    && !self.live
                    && !self.preparing
                    && !self.handoff_pending
                    && !self.validation.as_ref().is_some_and(|v| {
                        v.preparing
                            || self.task.validation.status
                                == crate::agent_task::TaskValidationStatus::Running
                    }))
            {
                RuntimeState::Conflict
            } else if self.live
                || self.preparing
                || self.validation.as_ref().is_some_and(|validation| {
                    validation.preparing
                        || self.task.validation.status
                            == crate::agent_task::TaskValidationStatus::Running
                })
            {
                RuntimeState::Running
            } else {
                RuntimeState::Idle
            },
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeCatalogEntry {
    pub id: ConversationId,
    pub task_id: TaskId,
    pub incarnation: String,
    pub sequence: u64,
}

/// Durable/presentation evidence is separate from the executable evidence held
/// only by the live host reducer. A redacted command is never marked exact.
pub(crate) fn safe_task_projection(task: &AgentTask, policy: NativePromptPolicy) -> AgentTask {
    let mut task = task.clone();
    if !policy.redact_secrets {
        return task;
    }
    let redact = crate::redact::redact_secrets;
    task.title = redact(&task.title);
    task.status_detail = task.status_detail.as_deref().map(redact);
    task.validation.status_detail = task.validation.status_detail.as_deref().map(redact);
    if let Some(context) = &mut task.source_context {
        if let Some(command) = &mut context.command {
            let safe = redact(command);
            if safe != *command {
                context.command_exact = false;
            }
            *command = safe;
        }
        context.output_text = redact(&context.output_text);
    }
    if let Some(brief) = &mut task.brief {
        brief.goal = redact(&brief.goal);
        if brief
            .validation
            .as_ref()
            .is_some_and(|recipe| redact(&recipe.command) != recipe.command)
        {
            brief.validation = None;
            task.status_detail=Some("Sensitive validation evidence is retained only by the current host. After a restart, supply a new explicit recipe or accept the work after review.".into());
        }
    }
    task
}
pub(crate) fn safe_snapshot_projection(snapshot: &NativeSnapshot) -> NativeSnapshot {
    let mut safe = snapshot.clone();
    let policy = NativePromptPolicy {
        share_command_context: false,
        redact_secrets: snapshot.redact_secrets,
    };
    let projected = safe_task_projection(&safe.task, policy);
    safe.validation_evidence_redacted |=
        safe.task.validation_command() != projected.validation_command();
    safe.task = projected;
    if let Some(validation) = &mut safe.validation {
        if policy.redact_secrets {
            validation.output = crate::redact::redact_secrets(&validation.output);
        }
    }
    if !policy.redact_secrets {
        return safe;
    }
    fn redact_value(value: &mut serde_json::Value, key: &str) {
        match value {
            serde_json::Value::String(text)
                if !matches!(
                    key,
                    "id" | "item_id"
                        | "turn_id"
                        | "local_turn_id"
                        | "provider_thread_id"
                        | "provider_turn_id"
                        | "displayed_turn_id"
                        | "approval_id"
                ) =>
            {
                *text = crate::redact::redact_secrets(text)
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    redact_value(item, key)
                }
            }
            serde_json::Value::Object(items) => {
                for (key, item) in items {
                    redact_value(item, key)
                }
            }
            _ => {}
        }
    }
    if let Some(view) = safe.view.take() {
        let mut encoded = serde_json::to_value(&view).unwrap_or_default();
        redact_value(&mut encoded, "");
        let mut projected: CodexAppServerViewSnapshot =
            serde_json::from_value(encoded).unwrap_or_default();
        if projected.pending_approvals != view.pending_approvals {
            projected.pending_approvals.clear();
            projected.last_error=Some("Sensitive approval evidence is redacted; no grant is exposed. Stop the task to inspect its exact provider history.".into());
        }
        for (old, item) in view.file_changes.iter().zip(&mut projected.file_changes) {
            for (old, change) in old.changes.iter().zip(&mut item.changes) {
                if change.path != old.path {
                    change.path_exact = false;
                }
                if change.diff != old.diff {
                    change.diff_exact = false;
                }
                if change.move_path != old.move_path {
                    change.move_path_exact = false;
                }
            }
        }
        safe.view = Some(projected);
    }
    if let Some(exit) = &mut safe.exit {
        exit.detail = exit.detail.as_deref().map(crate::redact::redact_secrets);
        exit.stderr_tail = crate::redact::redact_secrets(&exit.stderr_tail);
    }
    for event in &mut safe.events {
        for issue in &mut event.report.issues {
            issue.detail = crate::redact::redact_secrets(&issue.detail);
        }
        for completion in &mut event.report.completions {
            completion.detail = completion
                .detail
                .as_deref()
                .map(crate::redact::redact_secrets);
        }
    }
    safe
}

fn default_redaction() -> bool {
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeTaskMutation {
    Complete,
    AcceptReviewed,
    Archive,
    Restore,
}
/// Attach capability for an existing host-owned PTY. It contains no executable,
/// command, credentials, or permission to create a replacement process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedTerminalDescriptor {
    pub id: ConversationId,
    pub epoch: String,
    pub cwd: PathBuf,
    pub label: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostedValidationSnapshot {
    pub run_id: String,
    pub preparing: bool,
    pub terminal: Option<OwnedTerminalDescriptor>,
    pub output: String,
    pub output_truncated: bool,
}
