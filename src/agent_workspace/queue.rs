//! Durable dependency intent and read-only readiness. Ready is never a start
//! command: frontends must open the exact task and use its existing explicit
//! start/preflight/owner path. No timers, model calls or remote jobs run here.

use super::{ConversationId, ConversationKind, RuntimeState, WorkspaceRow};
use crate::agent_task::TaskStatus;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const WORKSPACE_QUEUE_LIMIT: usize = 256;
pub const WORKSPACE_QUEUE_DEPENDENCIES: usize = 32;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceExecutionLocation {
    #[default]
    Local,
    Ssh {
        host: String,
    },
    Dlc {
        job: String,
    },
}

impl WorkspaceExecutionLocation {
    pub fn label(&self) -> String {
        match self {
            Self::Local => "Local".into(),
            Self::Ssh { host } => format!("SSH · {host}"),
            Self::Dlc { job } => format!("DLC · {job}"),
        }
    }

    /// Native Workspace currently has only a local execution adapter. Retain
    /// remote intent visibly, but never reinterpret it as a local launch.
    pub fn unavailable_reason(&self) -> Option<&'static str> {
        match self {
            Self::Local => None,
            Self::Ssh { .. } => {
                Some("No verified SSH Agent Workspace execution adapter is configured")
            }
            Self::Dlc { .. } => {
                Some("No verified DLC Agent Workspace execution adapter is configured")
            }
        }
    }

    fn validate(&self) -> Result<(), String> {
        let value = match self {
            Self::Local => return Ok(()),
            Self::Ssh { host } => host,
            Self::Dlc { job } => job,
        };
        if value.is_empty() || value.len() > 512 || crate::review_input::validate(value).is_err() {
            return Err("Execution location requires a bounded, visible identity".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceQueueIntent {
    pub id: ConversationId,
    pub dependencies: Vec<ConversationId>,
    #[serde(default)]
    pub location: WorkspaceExecutionLocation,
}

impl WorkspaceQueueIntent {
    pub fn validate(&self) -> Result<(), String> {
        valid_task_id(&self.id)?;
        self.location.validate()?;
        if self.dependencies.len() > WORKSPACE_QUEUE_DEPENDENCIES {
            return Err(format!(
                "Use at most {WORKSPACE_QUEUE_DEPENDENCIES} task dependencies"
            ));
        }
        let mut seen = BTreeSet::new();
        for id in &self.dependencies {
            valid_task_id(id)?;
            if id == &self.id {
                return Err("A task cannot depend on itself".into());
            }
            if !seen.insert(id) {
                return Err("Task dependencies must be unique".into());
            }
        }
        Ok(())
    }
}

fn valid_task_id(id: &ConversationId) -> Result<(), String> {
    if !id.0.starts_with("jterm:task:v1:") || id.0.len() > 32768 || id.0.contains('\0') {
        return Err("The queue requires a source-qualified task identity".into());
    }
    let parts =
        id.0.strip_prefix("jterm:task:v1:")
            .and_then(|text| serde_json::from_str::<(String, String, String)>(text).ok());
    if !parts.is_some_and(|(host, namespace, task)| {
        !host.is_empty() && !namespace.is_empty() && !task.is_empty()
    }) {
        return Err("The queue requires an exact host, namespace and task identity".into());
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WorkspaceQueueChange {
    Upsert { intent: WorkspaceQueueIntent },
    Remove { id: ConversationId },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceQueueUpdate {
    pub expected_revision: u64,
    pub change: WorkspaceQueueChange,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceQueuedIntent {
    pub intent: WorkspaceQueueIntent,
    pub queued_at_ms: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceQueueSnapshot {
    pub revision: u64,
    pub entries: Vec<WorkspaceQueuedIntent>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceQueueState {
    Ready,
    WaitingDependencies,
    Active,
    Review,
    Completed,
    Interrupted,
    Blocked,
    Unavailable,
}

impl WorkspaceQueueState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "Ready to start explicitly",
            Self::WaitingDependencies => "Waiting for accepted dependencies",
            Self::Active => "Execution active",
            Self::Review => "Ready to review",
            Self::Completed => "Accepted",
            Self::Interrupted => "Outcome unconfirmed",
            Self::Blocked => "Blocked",
            Self::Unavailable => "Task details unavailable",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceQueueRow {
    pub id: ConversationId,
    pub title: String,
    pub state: WorkspaceQueueState,
    pub reason: Option<String>,
    pub blocked_by: Vec<ConversationId>,
    pub location: WorkspaceExecutionLocation,
}

impl WorkspaceQueueSnapshot {
    pub fn project(&self, rows: &[WorkspaceRow]) -> Vec<WorkspaceQueueRow> {
        let known: BTreeMap<_, _> = rows.iter().map(|row| (&row.id, row)).collect();
        self.entries.iter().map(|entry| {
            let intent = &entry.intent;
            let source = known.get(&intent.id).copied();
            let mut projected = WorkspaceQueueRow {
                id: intent.id.clone(),
                title: source.map(|row| row.title.clone()).unwrap_or_else(|| "Unavailable task".into()),
                state: WorkspaceQueueState::Unavailable,
                reason: None,
                blocked_by: Vec::new(),
                location: intent.location.clone(),
            };
            let Some(row) = source else {
                projected.reason = Some("Refresh or open the original task; absence is not completion".into());
                return projected;
            };
            if row.kind != ConversationKind::Task {
                projected.reason = Some("The source is not an engineering task".into());
            } else if row.facts.task_accepted || row.facts.task_status == Some(TaskStatus::Completed) {
                projected.state = WorkspaceQueueState::Completed;
            } else if row.facts.interrupted {
                projected.state = WorkspaceQueueState::Interrupted;
                projected.reason = Some("Inspect the previous execution before starting another".into());
            } else if row.runtime == RuntimeState::Conflict {
                projected.state = WorkspaceQueueState::Blocked;
                projected.reason = Some("Resolve the current execution owner first".into());
            } else if row.runtime.is_live() {
                projected.state = WorkspaceQueueState::Active;
            } else if let Some(reason) = intent.location.unavailable_reason() {
                projected.state = WorkspaceQueueState::Blocked;
                projected.reason = Some(reason.into());
            } else if row.archived {
                projected.state = WorkspaceQueueState::Blocked;
                projected.reason = Some("Restore the task before continuing".into());
            } else if row.facts.task_status == Some(TaskStatus::ReadyForReview) {
                projected.state = WorkspaceQueueState::Review;
            } else if row.facts.task_status != Some(TaskStatus::Created) {
                projected.reason = Some("Open the exact task and check its available continuation action".into());
            } else {
                projected.blocked_by = intent.dependencies.iter().filter(|id| {
                    known.get(id).is_none_or(|dependency| dependency.kind != ConversationKind::Task
                        || !(dependency.facts.task_accepted || dependency.facts.task_status == Some(TaskStatus::Completed)))
                }).cloned().collect();
                if !projected.blocked_by.is_empty() {
                    projected.state = WorkspaceQueueState::WaitingDependencies;
                    projected.reason = Some("Dependencies must be explicitly accepted; a passing test is insufficient".into());
                } else if let Some(worktree) = row.facts.worktree.as_deref()
                    .and_then(|value| super::query::normalized_absolute(std::path::Path::new(value))) {
                    let occupied = rows.iter().any(|other| {
                        if other.id == row.id || !other.runtime.is_live() { return false; }
                        let other_root = other.facts.worktree.as_deref()
                            .or_else(|| (other.kind == ConversationKind::Cli).then_some(other.cwd.as_str()))
                            .and_then(|value| super::query::normalized_absolute(std::path::Path::new(value)));
                        other_root.is_some_and(|other| other.starts_with(&worktree) || worktree.starts_with(other))
                    });
                    if occupied {
                        projected.state = WorkspaceQueueState::Blocked;
                        projected.reason = Some("Another observed execution uses this task worktree".into());
                    } else {
                        projected.state = WorkspaceQueueState::Ready;
                    }
                } else {
                    projected.reason = Some("Refresh task details to verify its worktree".into());
                }
            }
            projected
        }).collect()
    }

    pub(crate) fn apply_change(
        &mut self,
        change: &WorkspaceQueueChange,
        now: u64,
    ) -> Result<(), String> {
        let mut candidate = self.entries.clone();
        match change {
            WorkspaceQueueChange::Remove { id } => {
                valid_task_id(id)?;
                candidate.retain(|entry| entry.intent.id != *id);
            }
            WorkspaceQueueChange::Upsert { intent } => {
                intent.validate()?;
                if let Some(existing) = candidate
                    .iter_mut()
                    .find(|entry| entry.intent.id == intent.id)
                {
                    existing.intent = intent.clone();
                } else {
                    candidate.push(WorkspaceQueuedIntent {
                        intent: intent.clone(),
                        queued_at_ms: now,
                    });
                }
            }
        }
        if candidate.len() > WORKSPACE_QUEUE_LIMIT {
            return Err(format!(
                "The queue holds at most {WORKSPACE_QUEUE_LIMIT} tasks"
            ));
        }
        validate_graph(&candidate)?;
        self.entries = candidate;
        Ok(())
    }
}

fn validate_graph(entries: &[WorkspaceQueuedIntent]) -> Result<(), String> {
    let graph: BTreeMap<_, _> = entries
        .iter()
        .map(|entry| (&entry.intent.id, &entry.intent.dependencies))
        .collect();
    fn visit<'a>(
        id: &'a ConversationId,
        graph: &BTreeMap<&'a ConversationId, &'a Vec<ConversationId>>,
        pending: &mut BTreeSet<&'a ConversationId>,
        done: &mut BTreeSet<&'a ConversationId>,
    ) -> Result<(), String> {
        if done.contains(id) {
            return Ok(());
        }
        if !pending.insert(id) {
            return Err("Task dependency cycle rejected; the queue was not changed".into());
        }
        if let Some(dependencies) = graph.get(id) {
            for dependency in *dependencies {
                if let Some((&canonical, _)) = graph.get_key_value(dependency) {
                    visit(canonical, graph, pending, done)?;
                }
            }
        }
        pending.remove(id);
        done.insert(id);
        Ok(())
    }
    let mut done = BTreeSet::new();
    for id in graph.keys() {
        visit(id, &graph, &mut BTreeSet::new(), &mut done)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::TaskValidationStatus;
    use crate::agent_workspace::WorkspaceFacts;
    fn id(name: &str) -> ConversationId {
        ConversationId(format!(
            "jterm:task:v1:{}",
            serde_json::to_string(&("fixture-host", "fixture", name)).unwrap()
        ))
    }
    fn intent(name: &str, dependencies: &[&str]) -> WorkspaceQueueIntent {
        WorkspaceQueueIntent {
            id: id(name),
            dependencies: dependencies.iter().map(|name| id(name)).collect(),
            location: WorkspaceExecutionLocation::Local,
        }
    }
    fn row(name: &str, status: TaskStatus) -> WorkspaceRow {
        WorkspaceRow {
            id: id(name),
            kind: ConversationKind::Task,
            title: name.into(),
            provider: "Codex".into(),
            cwd: "/work/repo".into(),
            preview: String::new(),
            updated_at_ms: 1,
            archived: false,
            pinned: false,
            tags: Vec::new(),
            runtime: RuntimeState::Idle,
            facts: WorkspaceFacts {
                task_status: Some(status),
                worktree: Some(format!("/work/{name}")),
                ..WorkspaceFacts::default()
            },
        }
    }
    #[test]
    fn graph_cycles_are_atomic_and_missing_sources_are_not_ready() {
        let mut queue = WorkspaceQueueSnapshot::default();
        queue
            .apply_change(
                &WorkspaceQueueChange::Upsert {
                    intent: intent("a", &["b"]),
                },
                1,
            )
            .unwrap();
        let before = queue.clone();
        assert!(queue
            .apply_change(
                &WorkspaceQueueChange::Upsert {
                    intent: intent("b", &["a"])
                },
                2
            )
            .is_err());
        assert_eq!(queue, before);
        assert_eq!(
            queue.project(&[])[0].state,
            WorkspaceQueueState::Unavailable
        );
    }
    #[test]
    fn only_accepted_dependencies_release_readiness_and_live_worktree_is_exclusive() {
        let mut queue = WorkspaceQueueSnapshot::default();
        queue
            .apply_change(
                &WorkspaceQueueChange::Upsert {
                    intent: intent("a", &["b"]),
                },
                1,
            )
            .unwrap();
        let a = row("a", TaskStatus::Created);
        let mut b = row("b", TaskStatus::ReadyForReview);
        b.facts.validation_status = Some(TaskValidationStatus::Passed);
        assert_eq!(
            queue.project(&[a.clone(), b.clone()])[0].state,
            WorkspaceQueueState::WaitingDependencies
        );
        b.facts.task_status = Some(TaskStatus::Completed);
        assert_eq!(
            queue.project(&[a.clone(), b.clone()])[0].state,
            WorkspaceQueueState::Ready
        );
        let mut other = row("other", TaskStatus::Working);
        other.runtime = RuntimeState::Running;
        other.facts.worktree = a.facts.worktree.clone();
        assert_eq!(
            queue.project(&[a, b, other])[0].state,
            WorkspaceQueueState::Blocked
        );
    }
    #[test]
    fn a_live_cli_in_a_worktree_subdirectory_blocks_only_that_tree() {
        let mut queue = WorkspaceQueueSnapshot::default();
        queue
            .apply_change(
                &WorkspaceQueueChange::Upsert {
                    intent: intent("a", &[]),
                },
                1,
            )
            .unwrap();
        let a = row("a", TaskStatus::Created);
        let mut cli = row("terminal", TaskStatus::Working);
        cli.kind = ConversationKind::Cli;
        cli.facts = WorkspaceFacts::default();
        cli.runtime = RuntimeState::Attached;
        cli.cwd = "/work/a/src/./tests".into();
        assert_eq!(
            queue.project(&[a.clone(), cli.clone()])[0].state,
            WorkspaceQueueState::Blocked
        );
        cli.cwd = "/work/a-sibling".into();
        assert_eq!(
            queue.project(&[a, cli])[0].state,
            WorkspaceQueueState::Ready
        );
    }
    #[test]
    fn unconfigured_remote_intent_never_becomes_local_execution() {
        let mut queue = WorkspaceQueueSnapshot::default();
        let mut plan = intent("a", &[]);
        plan.location = WorkspaceExecutionLocation::Dlc {
            job: "fixture-job".into(),
        };
        queue
            .apply_change(&WorkspaceQueueChange::Upsert { intent: plan }, 1)
            .unwrap();
        let result = queue.project(&[row("a", TaskStatus::Created)]);
        assert_eq!(result[0].state, WorkspaceQueueState::Blocked);
        assert!(result[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("No verified DLC"));
    }
}
