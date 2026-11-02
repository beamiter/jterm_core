//! Read-only counts of observed workspace facts. Unknown catalog coverage is
//! explicit; process liveness is never inferred from recorded transcript text.
use super::{ConversationKind, WorkspaceAttention, WorkspaceRow};
use crate::agent_task::{TaskStatus, TaskValidationStatus};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceOverview {
    pub catalog_complete: bool,
    pub visible_conversations: usize,
    pub tasks: usize,
    pub active_cli: usize,
    pub active_api: usize,
    pub active_native: usize,
    pub active_task_terminals: usize,
    pub active_unknown_tasks: usize,
    pub validation_running: usize,
    pub accepted: usize,
    pub unconfirmed: usize,
    pub attention: Vec<(WorkspaceAttention, usize)>,
}

impl WorkspaceOverview {
    pub fn from_rows(rows: &[WorkspaceRow], catalog_complete: bool) -> Self {
        let mut overview = Self {
            catalog_complete,
            attention: WorkspaceAttention::ALL
                .into_iter()
                .map(|kind| (kind, 0))
                .collect(),
            ..Self::default()
        };
        for row in rows.iter().filter(|row| !row.archived) {
            overview.visible_conversations += 1;
            if let Some(kind) = row.attention() {
                if let Some((_, count)) = overview
                    .attention
                    .iter_mut()
                    .find(|(candidate, _)| *candidate == kind)
                {
                    *count += 1;
                }
            }
            if row.kind == ConversationKind::Task {
                overview.tasks += 1;
                overview.accepted += usize::from(
                    row.facts.task_accepted || row.facts.task_status == Some(TaskStatus::Completed),
                );
                overview.validation_running += usize::from(
                    row.runtime.is_live()
                        && row.facts.validation_status == Some(TaskValidationStatus::Running),
                );
            }
            if row.facts.interrupted {
                overview.unconfirmed += 1;
            }
            if row.runtime.is_live() {
                match row.kind {
                    ConversationKind::Cli => overview.active_cli += 1,
                    ConversationKind::Api => overview.active_api += 1,
                    ConversationKind::Task => {
                        if row.facts.agent_stream || row.facts.preparing {
                            overview.active_native += 1;
                        }
                        if row.facts.agent_terminal {
                            overview.active_task_terminals += 1;
                        }
                        if !row.facts.agent_stream
                            && !row.facts.preparing
                            && !row.facts.agent_terminal
                            && row.facts.validation_status != Some(TaskValidationStatus::Running)
                        {
                            overview.active_unknown_tasks += 1;
                        }
                    }
                }
            }
        }
        overview
    }
    pub fn attention_total(&self) -> usize {
        self.attention.iter().map(|(_, count)| count).sum()
    }
    pub fn resource_label(&self) -> String {
        let mut label = format!(
            "{} CLI · {} API · {} native active · {} validation running",
            self.active_cli, self.active_api, self.active_native, self.validation_running
        );
        if self.active_task_terminals > 0 {
            label.push_str(&format!(" · {} task terminals", self.active_task_terminals));
        }
        if self.active_unknown_tasks > 0 {
            label.push_str(&format!(
                " · {} task runtimes unspecified",
                self.active_unknown_tasks
            ));
        }
        if !self.catalog_complete {
            label.push_str(" · loaded catalog only");
        }
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_sessions::SessionStatus;
    use crate::agent_workspace::{ConversationId, RuntimeState, WorkspaceFacts};
    #[test]
    fn observed_counts_do_not_treat_recorded_work_as_live_or_hide_partial_coverage() {
        let rows = [WorkspaceRow {
            id: ConversationId("fixture".into()),
            kind: ConversationKind::Cli,
            title: "Old work".into(),
            provider: "Codex".into(),
            cwd: String::new(),
            preview: String::new(),
            updated_at_ms: 1,
            archived: false,
            pinned: false,
            tags: Vec::new(),
            runtime: RuntimeState::Idle,
            facts: WorkspaceFacts {
                recorded_status: Some(SessionStatus::Working),
                ..Default::default()
            },
        }];
        let summary = WorkspaceOverview::from_rows(&rows, false);
        assert_eq!(summary.active_cli, 0);
        assert_eq!(summary.attention_total(), 0);
        assert!(summary.resource_label().contains("loaded catalog only"));
    }

    #[test]
    fn validation_and_terminal_activity_are_not_counted_as_native_agents() {
        let mut row = WorkspaceRow {
            id: ConversationId("fixture-task".into()),
            kind: ConversationKind::Task,
            title: "Review".into(),
            provider: "Codex".into(),
            cwd: String::new(),
            preview: String::new(),
            updated_at_ms: 1,
            archived: false,
            pinned: false,
            tags: Vec::new(),
            runtime: RuntimeState::Running,
            facts: WorkspaceFacts {
                validation_status: Some(TaskValidationStatus::Running),
                validation_terminal: true,
                ..Default::default()
            },
        };
        let summary = WorkspaceOverview::from_rows(&[row.clone()], true);
        assert_eq!(summary.validation_running, 1);
        assert_eq!(summary.active_native, 0);
        assert_eq!(summary.active_unknown_tasks, 0);
        row.facts.validation_status = Some(TaskValidationStatus::Passed);
        row.facts.validation_terminal = false;
        row.facts.agent_terminal = true;
        let summary = WorkspaceOverview::from_rows(&[row.clone()], true);
        assert_eq!(summary.active_task_terminals, 1);
        assert_eq!(summary.active_native, 0);
        row.runtime = RuntimeState::Idle;
        row.facts.validation_status = Some(TaskValidationStatus::Running);
        assert_eq!(
            WorkspaceOverview::from_rows(&[row], true).validation_running,
            0
        );
    }
}
