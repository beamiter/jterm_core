//! Pure, toolkit-independent task-list projection. TaskManager remains the sole
//! lifecycle authority; activity facts never imply completion or grant actions.

use super::{AgentProvider, AgentTask, TaskId, TaskRuntimeKind, TaskStatus, TaskValidationStatus};
use crate::review_input::safe_inline_display;
use std::path::Path;

pub const DASHBOARD_TITLE_MAX_BYTES: usize = 256;
pub const DASHBOARD_DETAIL_MAX_BYTES: usize = 1024;
pub const DASHBOARD_QUERY_MAX_BYTES: usize = 256;

/// Mutually exclusive lifecycle buckets, independent of process activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DashboardGroup {
    Created,
    Active,
    PendingApproval,
    WaitingForHuman,
    Review,
    Done,
    Failed,
    Cancelled,
    Archived,
}

impl DashboardGroup {
    pub fn from_status(status: TaskStatus) -> Self {
        match status {
            TaskStatus::Created => Self::Created,
            TaskStatus::Starting | TaskStatus::Working => Self::Active,
            TaskStatus::WaitingForApproval => Self::PendingApproval,
            TaskStatus::WaitingForHuman => Self::WaitingForHuman,
            TaskStatus::ReadyForReview => Self::Review,
            TaskStatus::Completed => Self::Done,
            TaskStatus::Failed => Self::Failed,
            TaskStatus::Cancelled => Self::Cancelled,
            TaskStatus::Archived => Self::Archived,
        }
    }
}

/// Facts resolved by the adapter from existing runtime/session owners. A saved
/// terminal binding alone does not establish that a terminal still exists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DashboardActivity {
    pub has_agent_terminal: bool,
    pub has_validation_terminal: bool,
    pub has_active_agent_stream: bool,
    pub native_preparing: bool,
}

/// Compatible with the existing task-row adapters, with bounded display text.
/// These are presentation facts, never action eligibility or execution state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DashboardRow {
    pub id: TaskId,
    pub title: String,
    pub provider: AgentProvider,
    pub status: TaskStatus,
    pub runtime_kind: TaskRuntimeKind,
    pub branch: String,
    pub repo_label: String,
    pub activity: DashboardActivity,
    pub validation_status: TaskValidationStatus,
    pub validation_attempt: u64,
    pub needs_attention: bool,
    pub status_detail: Option<String>,
    pub updated_at_ms: u64,
}

impl DashboardRow {
    pub fn from_task(task: &AgentTask, activity: DashboardActivity) -> Self {
        Self {
            id: task.id,
            title: safe_inline_display(&task.title, DASHBOARD_TITLE_MAX_BYTES),
            provider: task.provider,
            status: task.status,
            runtime_kind: task.runtime_kind,
            branch: safe_inline_display(&task.branch, DASHBOARD_TITLE_MAX_BYTES),
            repo_label: safe_inline_display(
                &task.repo_root.to_string_lossy(),
                DASHBOARD_DETAIL_MAX_BYTES,
            ),
            activity,
            validation_status: task.validation.status,
            validation_attempt: task.validation.attempt,
            needs_attention: task.status != TaskStatus::Archived && task.needs_attention(),
            status_detail: task
                .status_detail
                .as_deref()
                .map(|text| safe_inline_display(text, DASHBOARD_DETAIL_MAX_BYTES)),
            updated_at_ms: task.updated_at_ms,
        }
    }

    pub fn group(&self) -> DashboardGroup {
        DashboardGroup::from_status(self.status)
    }

    /// Activity is orthogonal: a review task can retain a live native stream.
    pub fn is_running(&self) -> bool {
        self.status.is_running()
            || self.validation_status == TaskValidationStatus::Running
            || self.activity.has_active_agent_stream
            || self.activity.native_preparing
    }

    pub fn status_line(&self) -> String {
        let base = if self.activity.native_preparing
            && !matches!(
                self.status,
                TaskStatus::WaitingForApproval | TaskStatus::WaitingForHuman
            ) {
            format!("Preparing native {}", self.provider.display_name())
        } else {
            self.status.label().to_owned()
        };
        let line = if self.validation_status == TaskValidationStatus::NotRun {
            base
        } else {
            format!("{base} · validation {}", self.validation_status.label())
        };
        safe_inline_display(&line, DASHBOARD_DETAIL_MAX_BYTES)
    }

    fn rank(&self) -> u8 {
        if self.needs_attention {
            0
        } else if self.is_running() {
            1
        } else {
            2
        }
    }
}

/// All constraints are ANDed. Repository matching uses the original exact path
/// (no filesystem access/canonicalization); text searches only displayed fields.
#[derive(Clone, Copy, Debug, Default)]
pub struct DashboardFilter<'a> {
    pub provider: Option<AgentProvider>,
    pub repo_root: Option<&'a Path>,
    pub text: &'a str,
    pub group: Option<DashboardGroup>,
    pub view: DashboardView,
}

/// Shared quick filters for native task panels. Running and attention can
/// overlap; neither filter changes the underlying lifecycle status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DashboardView {
    #[default]
    All,
    NeedsAttention,
    Running,
    NotStarted,
    Review,
    Finished,
}

impl DashboardView {
    fn matches(self, row: &DashboardRow) -> bool {
        match self {
            Self::All => true,
            Self::NeedsAttention => row.needs_attention,
            Self::Running => row.is_running(),
            Self::NotStarted => row.status == TaskStatus::Created,
            Self::Review => row.status == TaskStatus::ReadyForReview,
            Self::Finished => matches!(
                row.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            ),
        }
    }
}

/// Lifecycle counts are disjoint. Attention and running intentionally overlap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DashboardCounts {
    pub total: usize,
    pub created: usize,
    pub active: usize,
    pub pending_approval: usize,
    pub waiting_for_human: usize,
    pub review: usize,
    pub done: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub archived: usize,
    pub attention: usize,
    pub running: usize,
}

impl DashboardCounts {
    fn add(&mut self, row: &DashboardRow) {
        self.total += 1;
        match row.group() {
            DashboardGroup::Created => self.created += 1,
            DashboardGroup::Active => self.active += 1,
            DashboardGroup::PendingApproval => self.pending_approval += 1,
            DashboardGroup::WaitingForHuman => self.waiting_for_human += 1,
            DashboardGroup::Review => self.review += 1,
            DashboardGroup::Done => self.done += 1,
            DashboardGroup::Failed => self.failed += 1,
            DashboardGroup::Cancelled => self.cancelled += 1,
            DashboardGroup::Archived => self.archived += 1,
        }
        self.attention += usize::from(row.needs_attention);
        self.running += usize::from(row.is_running());
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DashboardSnapshot {
    pub rows: Vec<DashboardRow>,
    /// Counts after provider/repository/text filters, before the group filter.
    /// This keeps group badges useful even when one group is selected.
    pub counts: DashboardCounts,
}

impl DashboardSnapshot {
    /// Filter an already bounded, domain-derived snapshot without granting
    /// access to the task manager. Native views keep selection separately.
    pub fn filter_rows(rows: &[DashboardRow], text: &str, view: DashboardView) -> Self {
        let query = safe_inline_display(text.trim(), DASHBOARD_QUERY_MAX_BYTES).to_lowercase();
        let mut snapshot = Self::default();
        for row in rows {
            let matches_text = query.is_empty()
                || [
                    row.title.as_str(),
                    row.branch.as_str(),
                    row.repo_label.as_str(),
                    row.provider.display_name(),
                    row.status_detail.as_deref().unwrap_or_default(),
                ]
                .iter()
                .any(|field| field.to_lowercase().contains(&query));
            if !matches_text {
                continue;
            }
            snapshot.counts.add(row);
            if view.matches(row) {
                snapshot.rows.push(row.clone());
            }
        }
        snapshot.rows.sort_by_cached_key(|row| {
            (
                row.rank(),
                std::cmp::Reverse(row.updated_at_ms),
                row.id.to_string(),
            )
        });
        snapshot
    }

    pub fn project<'a>(
        tasks: impl IntoIterator<Item = &'a AgentTask>,
        activity: impl Fn(TaskId) -> DashboardActivity,
        filter: DashboardFilter<'_>,
    ) -> Self {
        let query =
            safe_inline_display(filter.text.trim(), DASHBOARD_QUERY_MAX_BYTES).to_lowercase();
        let mut snapshot = Self::default();
        for task in tasks {
            if filter
                .provider
                .is_some_and(|provider| task.provider != provider)
                || filter.repo_root.is_some_and(|repo| task.repo_root != repo)
            {
                continue;
            }
            let row = DashboardRow::from_task(task, activity(task.id));
            let matches_text = query.is_empty()
                || [
                    row.title.as_str(),
                    row.branch.as_str(),
                    row.repo_label.as_str(),
                    row.provider.display_name(),
                    row.status_detail.as_deref().unwrap_or_default(),
                ]
                .iter()
                .any(|field| field.to_lowercase().contains(&query));
            if !matches_text {
                continue;
            }
            snapshot.counts.add(&row);
            if filter.group.is_none_or(|group| row.group() == group) && filter.view.matches(&row) {
                snapshot.rows.push(row);
            }
        }
        snapshot.rows.sort_by_cached_key(|row| {
            (
                row.rank(),
                std::cmp::Reverse(row.updated_at_ms),
                row.id.to_string(),
            )
        });
        snapshot
    }

    /// Resolve a stable identity after filtering/sorting; never choose another
    /// task merely because the previous selection is hidden or removed.
    pub fn selected_index(&self, selected: Option<TaskId>) -> Option<usize> {
        selected.and_then(|id| self.rows.iter().position(|row| row.id == id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::{NewTask, TaskManager};

    fn task(status: TaskStatus) -> AgentTask {
        let mut manager = TaskManager::new();
        let id = manager
            .create(NewTask {
                title: "Fix dashboard".into(),
                provider: AgentProvider::Codex,
                repo_root: "/repo".into(),
                worktree_path: "/worktree".into(),
                branch: "fix/dashboard".into(),
                base_commit: "a".repeat(40),
                source_context: None,
            })
            .unwrap();
        let mut task = manager.get(id).unwrap().clone();
        task.status = status;
        task.updated_at_ms = 1;
        task
    }

    fn project(tasks: &[AgentTask], filter: DashboardFilter<'_>) -> DashboardSnapshot {
        DashboardSnapshot::project(tasks, |_| DashboardActivity::default(), filter)
    }

    #[test]
    fn every_status_has_an_explicit_disjoint_bucket() {
        let cases = [
            (TaskStatus::Created, DashboardGroup::Created),
            (TaskStatus::Starting, DashboardGroup::Active),
            (TaskStatus::Working, DashboardGroup::Active),
            (
                TaskStatus::WaitingForApproval,
                DashboardGroup::PendingApproval,
            ),
            (TaskStatus::WaitingForHuman, DashboardGroup::WaitingForHuman),
            (TaskStatus::ReadyForReview, DashboardGroup::Review),
            (TaskStatus::Completed, DashboardGroup::Done),
            (TaskStatus::Failed, DashboardGroup::Failed),
            (TaskStatus::Cancelled, DashboardGroup::Cancelled),
            (TaskStatus::Archived, DashboardGroup::Archived),
        ];
        let tasks: Vec<_> = cases
            .iter()
            .map(|(status, group)| {
                assert_eq!(DashboardGroup::from_status(*status), *group);
                task(*status)
            })
            .collect();
        let before = tasks.clone();
        let snapshot = project(&tasks, DashboardFilter::default());
        assert_eq!(tasks, before);
        assert_eq!(
            snapshot.counts,
            DashboardCounts {
                total: 10,
                created: 1,
                active: 2,
                pending_approval: 1,
                waiting_for_human: 1,
                review: 1,
                done: 1,
                failed: 1,
                cancelled: 1,
                archived: 1,
                attention: 4,
                running: 4,
            }
        );
    }

    #[test]
    fn validation_and_retained_streams_do_not_rewrite_lifecycle() {
        for status in [
            TaskStatus::ReadyForReview,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            let mut task = task(status);
            for validation in [
                TaskValidationStatus::Running,
                TaskValidationStatus::Passed,
                TaskValidationStatus::Failed,
                TaskValidationStatus::Inconclusive,
                TaskValidationStatus::Cancelled,
            ] {
                task.validation.status = validation;
                let before = task.clone();
                let row = DashboardRow::from_task(
                    &task,
                    DashboardActivity {
                        has_active_agent_stream: true,
                        ..Default::default()
                    },
                );
                assert_eq!(row.group(), DashboardGroup::from_status(status));
                assert_eq!(row.needs_attention, task.needs_attention());
                assert!(row.is_running());
                assert_eq!(task, before);
            }
        }
    }

    #[test]
    fn preparation_is_provider_neutral_and_not_completion() {
        for provider in AgentProvider::ALL {
            let mut task = task(TaskStatus::Created);
            task.provider = provider;
            let row = DashboardRow::from_task(
                &task,
                DashboardActivity {
                    native_preparing: true,
                    ..Default::default()
                },
            );
            assert!(row.status_line().contains(provider.display_name()));
            assert!(row.is_running());
            assert_eq!(row.group(), DashboardGroup::Created);
        }
    }

    #[test]
    fn filtering_is_conjunctive_and_counts_precede_group_selection() {
        let mut tasks = vec![
            task(TaskStatus::Created),
            task(TaskStatus::Failed),
            task(TaskStatus::Completed),
        ];
        tasks[2].provider = AgentProvider::Claude;
        let filter = DashboardFilter {
            provider: Some(AgentProvider::Codex),
            repo_root: Some(Path::new("/repo")),
            text: "DASHBOARD",
            group: Some(DashboardGroup::Created),
            view: DashboardView::All,
        };
        let result = project(&tasks, filter);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].id, tasks[0].id);
        assert_eq!(result.counts.total, 2);
        assert_eq!(result.counts.failed, 1);
        assert_eq!(result.selected_index(Some(tasks[1].id)), None);
        assert!(project(
            &tasks,
            DashboardFilter {
                repo_root: Some(Path::new("/repo-other")),
                ..filter
            }
        )
        .rows
        .is_empty());
        assert!(project(
            &tasks,
            DashboardFilter {
                text: "missing",
                ..filter
            }
        )
        .rows
        .is_empty());
    }

    #[test]
    fn ordering_and_selection_use_identity_not_input_order() {
        let mut tasks = vec![
            task(TaskStatus::Created),
            task(TaskStatus::Working),
            task(TaskStatus::Failed),
            task(TaskStatus::Failed),
        ];
        let first = project(&tasks, DashboardFilter::default());
        tasks.reverse();
        let reversed = project(&tasks, DashboardFilter::default());
        assert_eq!(first, reversed);
        assert_eq!(first.rows[0].status, TaskStatus::Failed);
        assert_eq!(first.rows[1].status, TaskStatus::Failed);
        assert!(first.rows[0].id.to_string() < first.rows[1].id.to_string());
        assert_eq!(first.rows[2].status, TaskStatus::Working);
        assert_eq!(first.selected_index(Some(first.rows[2].id)), Some(2));
        assert_eq!(first.selected_index(Some(TaskId::new())), None);
        assert_eq!(first.selected_index(None), None);
        tasks[0].updated_at_ms = 99;
        assert_eq!(
            project(&tasks, DashboardFilter::default()).rows[0].id,
            tasks[0].id
        );
    }

    #[test]
    fn display_text_is_bounded_and_sanitized_without_mutating_source() {
        let mut task = task(TaskStatus::Created);
        let hostile = format!("\u{1b}\n\u{202e}{}", "界".repeat(3000));
        task.title = hostile.clone();
        task.branch = hostile.clone();
        task.status_detail = Some(hostile.clone());
        task.repo_root = Path::new("/").join(&hostile);
        let before = task.clone();
        let row = DashboardRow::from_task(&task, DashboardActivity::default());
        for (text, max) in [
            (row.title.as_str(), DASHBOARD_TITLE_MAX_BYTES),
            (row.branch.as_str(), DASHBOARD_TITLE_MAX_BYTES),
            (row.repo_label.as_str(), DASHBOARD_DETAIL_MAX_BYTES),
            (
                row.status_detail.as_deref().unwrap(),
                DASHBOARD_DETAIL_MAX_BYTES,
            ),
        ] {
            assert!(text.len() <= max);
            assert!(!text.chars().any(char::is_control));
            assert!(!text.contains('\u{202e}'));
        }
        assert_eq!(task, before);
    }

    #[test]
    fn saved_terminal_binding_is_not_live_activity() {
        let mut task = task(TaskStatus::Created);
        task.terminal_session_id = Some("old-session".into());
        let row = DashboardRow::from_task(&task, DashboardActivity::default());
        assert!(!row.activity.has_agent_terminal);
        assert!(!row.is_running());
        assert_eq!(
            project(&[], DashboardFilter::default()),
            DashboardSnapshot::default()
        );
    }
    #[test]
    fn archived_validation_failure_does_not_request_attention() {
        let mut task = task(TaskStatus::Archived);
        task.validation.status = TaskValidationStatus::Failed;
        let row = DashboardRow::from_task(&task, DashboardActivity::default());
        assert!(!row.needs_attention);
        assert_eq!(row.group(), DashboardGroup::Archived);
    }
    #[test]
    fn preparation_cannot_hide_required_human_action() {
        for status in [TaskStatus::WaitingForApproval, TaskStatus::WaitingForHuman] {
            let row = DashboardRow::from_task(
                &task(status),
                DashboardActivity {
                    native_preparing: true,
                    ..DashboardActivity::default()
                },
            );
            assert!(row.needs_attention);
            assert!(row.status_line().starts_with(status.label()));
        }
    }
    #[test]
    fn quick_views_keep_authoritative_counts_and_hidden_selection() {
        let tasks = vec![
            task(TaskStatus::Created),
            task(TaskStatus::WaitingForApproval),
            task(TaskStatus::ReadyForReview),
            task(TaskStatus::Failed),
            task(TaskStatus::Cancelled),
            task(TaskStatus::Completed),
            task(TaskStatus::Working),
        ];
        for (view, count) in [
            (DashboardView::All, 7),
            (DashboardView::NeedsAttention, 3),
            (DashboardView::Running, 2),
            (DashboardView::NotStarted, 1),
            (DashboardView::Review, 1),
            (DashboardView::Finished, 3),
        ] {
            let snapshot = project(
                &tasks,
                DashboardFilter {
                    view,
                    ..DashboardFilter::default()
                },
            );
            assert_eq!(snapshot.rows.len(), count);
            assert_eq!(snapshot.counts.total, tasks.len());
            if view != DashboardView::All && view != DashboardView::NotStarted {
                assert_eq!(snapshot.selected_index(Some(tasks[0].id)), None);
            }
        }
    }
    #[test]
    fn snapshot_filter_matches_direct_projection() {
        let tasks = vec![
            task(TaskStatus::Created),
            task(TaskStatus::WaitingForApproval),
            task(TaskStatus::ReadyForReview),
            task(TaskStatus::Failed),
            task(TaskStatus::Completed),
        ];
        let all = project(&tasks, DashboardFilter::default());
        for query in ["", "dashboard", "Codex", "missing", "/repo"] {
            for view in [
                DashboardView::All,
                DashboardView::NeedsAttention,
                DashboardView::Running,
                DashboardView::NotStarted,
                DashboardView::Review,
                DashboardView::Finished,
            ] {
                assert_eq!(
                    DashboardSnapshot::filter_rows(&all.rows, query, view),
                    project(
                        &tasks,
                        DashboardFilter {
                            text: query,
                            view,
                            ..DashboardFilter::default()
                        }
                    )
                );
            }
        }
    }
}
