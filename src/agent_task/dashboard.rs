//! Pure, toolkit-independent task-list projection. TaskManager remains the sole
//! lifecycle authority; activity facts never imply completion or grant actions.

use super::query::DashboardQuery;
use super::{AgentProvider, AgentTask, TaskId, TaskRuntimeKind, TaskStatus, TaskValidationStatus};
use crate::review_input::safe_inline_display;
use std::path::Path;

pub const DASHBOARD_TITLE_MAX_BYTES: usize = 256;
pub const DASHBOARD_DETAIL_MAX_BYTES: usize = 1024;
pub const DASHBOARD_QUERY_MAX_BYTES: usize = 256;

/// Disjoint human-action queues. Approval, input and a failed run take
/// precedence over an old validation result; running validation is not review.
/// These labels do not grant the ability to execute any task action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DashboardAttentionKind {
    Approval,
    Input,
    Validation,
    Review,
    Failed,
}

impl DashboardAttentionKind {
    pub const ALL: [Self; 5] = [
        Self::Approval,
        Self::Input,
        Self::Validation,
        Self::Review,
        Self::Failed,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Approval => "Approvals",
            Self::Input => "Needs reply",
            Self::Validation => "Validation issues",
            Self::Review => "Ready to review",
            Self::Failed => "Failed tasks",
        }
    }

    pub fn query_value(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Input => "input",
            Self::Validation => "validation",
            Self::Review => "review",
            Self::Failed => "failed",
        }
    }
}

/// Counts within an already searched scope, before quick-view narrowing.
/// One row contributes to at most one reason; this never grants an action.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DashboardAttentionCounts([usize; 5]);

impl DashboardAttentionCounts {
    pub fn get(self, kind: DashboardAttentionKind) -> usize {
        self.0[match kind {
            DashboardAttentionKind::Approval => 0,
            DashboardAttentionKind::Input => 1,
            DashboardAttentionKind::Validation => 2,
            DashboardAttentionKind::Review => 3,
            DashboardAttentionKind::Failed => 4,
        }]
    }

    fn add(&mut self, kind: DashboardAttentionKind) {
        let index = match kind {
            DashboardAttentionKind::Approval => 0,
            DashboardAttentionKind::Input => 1,
            DashboardAttentionKind::Validation => 2,
            DashboardAttentionKind::Review => 3,
            DashboardAttentionKind::Failed => 4,
        };
        self.0[index] += 1;
    }
}

impl std::fmt::Display for DashboardAttentionKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

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

    pub fn attention_kind(&self) -> Option<DashboardAttentionKind> {
        if !self.needs_attention
            || self.status == TaskStatus::Archived
            || self.validation_status == TaskValidationStatus::Running
        {
            return None;
        }
        match self.status {
            TaskStatus::WaitingForApproval => Some(DashboardAttentionKind::Approval),
            TaskStatus::WaitingForHuman => Some(DashboardAttentionKind::Input),
            TaskStatus::Failed => Some(DashboardAttentionKind::Failed),
            _ if self.validation_status.needs_attention() => {
                Some(DashboardAttentionKind::Validation)
            }
            TaskStatus::ReadyForReview => Some(DashboardAttentionKind::Review),
            _ => None,
        }
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

/// Presentation ordering never changes lifecycle or action eligibility.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DashboardSort {
    #[default]
    Attention,
    Recent,
    Oldest,
    Title,
    Project,
    /// Attention first, with older updates first within each activity bucket.
    /// This uses recorded update time, not an inferred approval waiting time.
    AttentionOldest,
}
impl DashboardSort {
    pub const ALL: [Self; 6] = [
        Self::Attention,
        Self::Recent,
        Self::Oldest,
        Self::Title,
        Self::Project,
        Self::AttentionOldest,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Attention => "Needs you first",
            Self::Recent => "Recently updated",
            Self::Oldest => "Oldest update",
            Self::Title => "Task name",
            Self::Project => "Project",
            Self::AttentionOldest => "Needs you · oldest update",
        }
    }
}
impl std::fmt::Display for DashboardSort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
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
        let mut snapshot = Self::filter_iter_unsorted(rows, text, view);
        snapshot.sort_rows(DashboardSort::default());
        snapshot
    }

    /// Search borrowed candidates once, cloning only retained rows. The caller
    /// chooses the final order after narrowing the resulting scope.
    pub fn filter_iter_unsorted<'a>(
        rows: impl IntoIterator<Item = &'a DashboardRow>,
        text: &str,
        view: DashboardView,
    ) -> Self {
        let Ok(query) = DashboardQuery::parse(text) else {
            return Self::default();
        };
        let mut snapshot = Self::default();
        for row in rows {
            if !query.matches(row) {
                continue;
            }
            snapshot.counts.add(row);
            if view.matches(row) {
                snapshot.rows.push(row.clone());
            }
        }
        snapshot
    }

    pub fn project<'a>(
        tasks: impl IntoIterator<Item = &'a AgentTask>,
        activity: impl Fn(TaskId) -> DashboardActivity,
        filter: DashboardFilter<'_>,
    ) -> Self {
        let mut snapshot = Self::project_unsorted(tasks, activity, filter);
        snapshot.sort_rows(DashboardSort::default());
        snapshot
    }

    /// The same domain projection and counts as `project`, without an
    /// intermediate sort when a frontend will select its own final order.
    pub fn project_unsorted<'a>(
        tasks: impl IntoIterator<Item = &'a AgentTask>,
        activity: impl Fn(TaskId) -> DashboardActivity,
        filter: DashboardFilter<'_>,
    ) -> Self {
        let Ok(query) = DashboardQuery::parse(filter.text) else {
            return Self::default();
        };
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
            if !query.matches(&row) {
                continue;
            }
            snapshot.counts.add(&row);
            if filter.group.is_none_or(|group| row.group() == group) && filter.view.matches(&row) {
                snapshot.rows.push(row);
            }
        }
        snapshot
    }

    pub fn attention_counts(&self) -> DashboardAttentionCounts {
        let mut counts = DashboardAttentionCounts::default();
        for row in &self.rows {
            if let Some(kind) = row.attention_kind() {
                counts.add(kind);
            }
        }
        counts
    }

    /// Narrow an existing searched scope without parsing another query or
    /// recounting its badges. Selection remains owned by the frontend.
    pub fn apply_view(
        &mut self,
        view: DashboardView,
        attention: Option<DashboardAttentionKind>,
        sort: DashboardSort,
    ) {
        self.rows.retain(|row| {
            view.matches(row) && attention.is_none_or(|kind| row.attention_kind() == Some(kind))
        });
        self.sort_rows(sort);
    }

    pub fn view(
        &self,
        view: DashboardView,
        attention: Option<DashboardAttentionKind>,
        sort: DashboardSort,
    ) -> Self {
        let mut snapshot = Self {
            rows: self
                .rows
                .iter()
                .filter(|row| {
                    view.matches(row)
                        && attention.is_none_or(|kind| row.attention_kind() == Some(kind))
                })
                .cloned()
                .collect(),
            counts: self.counts.clone(),
        };
        snapshot.sort_rows(sort);
        snapshot
    }

    /// Resolve a stable identity after filtering/sorting; never choose another
    /// task merely because the previous selection is hidden or removed.
    pub fn selected_index(&self, selected: Option<TaskId>) -> Option<usize> {
        selected.and_then(|id| self.rows.iter().position(|row| row.id == id))
    }
    pub fn sort_rows(&mut self, sort: DashboardSort) {
        match sort {
            DashboardSort::Title | DashboardSort::Project => {
                self.rows.sort_by_cached_key(|row| {
                    let (name, rank) = if sort == DashboardSort::Title {
                        (row.title.to_lowercase(), 0)
                    } else {
                        (row.repo_label.to_lowercase(), row.rank())
                    };
                    (name, rank, u64::MAX - row.updated_at_ms, row.id.sort_key())
                });
            }
            _ => self.rows.sort_unstable_by_key(|row| {
                let recent = u64::MAX - row.updated_at_ms;
                let (rank, time) = match sort {
                    DashboardSort::Attention => (row.rank(), recent),
                    DashboardSort::Recent => (0, recent),
                    DashboardSort::Oldest => (0, row.updated_at_ms),
                    DashboardSort::AttentionOldest => (row.rank(), row.updated_at_ms),
                    DashboardSort::Title | DashboardSort::Project => unreachable!(),
                };
                (rank, time, row.id.sort_key())
            }),
        }
    }

    /// Explicit navigation only. Filtering/refresh must never call this to
    /// silently replace a hidden selected task with a different identity.
    pub fn step_selection(&self, selected: Option<TaskId>, forward: bool) -> Option<TaskId> {
        let length = self.rows.len();
        if length == 0 {
            return None;
        }
        let next = match (self.selected_index(selected), forward) {
            (Some(index), true) => (index + 1) % length,
            (Some(index), false) => (index + length - 1) % length,
            (None, true) => 0,
            (None, false) => length - 1,
        };
        Some(self.rows[next].id)
    }
    pub fn next_attention(&self, selected: Option<TaskId>) -> Option<TaskId> {
        self.step_attention(selected, true)
    }

    pub fn previous_attention(&self, selected: Option<TaskId>) -> Option<TaskId> {
        self.step_attention(selected, false)
    }

    /// Explicit bidirectional navigation in the current visible order. A hidden
    /// selection starts at the corresponding edge; no task is acted upon.
    pub fn step_attention(&self, selected: Option<TaskId>, forward: bool) -> Option<TaskId> {
        let length = self.rows.len();
        if length == 0 {
            return None;
        }
        let start = match (self.selected_index(selected), forward) {
            (Some(index), true) => (index + 1) % length,
            (Some(index), false) => (index + length - 1) % length,
            (None, true) => 0,
            (None, false) => length - 1,
        };
        (0..length)
            .map(|offset| {
                let index = if forward {
                    (start + offset) % length
                } else {
                    (start + length - offset) % length
                };
                &self.rows[index]
            })
            .find(|row| row.needs_attention)
            .map(|row| row.id)
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
    fn management_sort_and_navigation_only_select_visible_stable_identities() {
        let mut a = task(TaskStatus::WaitingForApproval);
        a.title = "Zulu review".into();
        a.repo_root = "/z-project".into();
        a.updated_at_ms = 10;
        let mut b = task(TaskStatus::Working);
        b.title = "Alpha work".into();
        b.repo_root = "/a-project".into();
        b.updated_at_ms = 30;
        let mut c = task(TaskStatus::WaitingForHuman);
        c.title = "Beta input".into();
        c.repo_root = "/a-project".into();
        c.updated_at_ms = 20;
        let tasks = vec![a, b, c];
        let original = tasks.clone();
        let mut snapshot = project(&tasks, DashboardFilter::default());
        let counts = snapshot.counts.clone();
        assert_eq!(
            snapshot.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [tasks[2].id, tasks[0].id, tasks[1].id]
        );
        snapshot.sort_rows(DashboardSort::Recent);
        assert_eq!(snapshot.rows[0].id, tasks[1].id);
        snapshot.sort_rows(DashboardSort::Oldest);
        assert_eq!(snapshot.rows[0].id, tasks[0].id);
        snapshot.sort_rows(DashboardSort::Title);
        assert_eq!(
            snapshot.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [tasks[1].id, tasks[2].id, tasks[0].id]
        );
        assert_eq!(
            snapshot.step_selection(Some(tasks[0].id), true),
            Some(tasks[1].id)
        );
        assert_eq!(
            snapshot.step_selection(Some(tasks[1].id), false),
            Some(tasks[0].id)
        );
        assert_eq!(
            snapshot.next_attention(Some(tasks[0].id)),
            Some(tasks[2].id)
        );
        snapshot.sort_rows(DashboardSort::Project);
        assert_eq!(snapshot.rows[0].id, tasks[2].id);
        assert_eq!(snapshot.counts, counts);
        assert_eq!(tasks, original);
        let filtered = project(
            &tasks,
            DashboardFilter {
                text: "alpha",
                ..Default::default()
            },
        );
        assert_eq!(filtered.selected_index(Some(tasks[0].id)), None);
        assert_eq!(filtered.next_attention(Some(tasks[0].id)), None);
        assert_eq!(
            filtered.step_selection(Some(tasks[0].id), true),
            Some(tasks[1].id)
        );
        assert_eq!(
            DashboardSnapshot::default().step_selection(None, true),
            None
        );
        assert_eq!(DashboardSnapshot::default().next_attention(None), None);
    }

    #[test]
    fn attention_age_sort_keeps_live_and_attention_facts_separate() {
        let mut tasks = vec![
            task(TaskStatus::WaitingForHuman),
            task(TaskStatus::WaitingForApproval),
            task(TaskStatus::Working),
            task(TaskStatus::Completed),
        ];
        for (task, updated) in tasks.iter_mut().zip([30, 20, 10, 1]) {
            task.updated_at_ms = updated;
        }
        let before = tasks.clone();
        let mut snapshot = project(&tasks, DashboardFilter::default());
        let counts = snapshot.counts.clone();
        assert_eq!(snapshot.rows[0].id, tasks[0].id);
        snapshot.sort_rows(DashboardSort::AttentionOldest);
        assert_eq!(
            snapshot.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [tasks[1].id, tasks[0].id, tasks[2].id, tasks[3].id]
        );
        assert_eq!(snapshot.counts, counts);
        assert_eq!(snapshot.selected_index(Some(tasks[0].id)), Some(1));
        assert_eq!(tasks, before);
        tasks.reverse();
        let mut reversed = project(&tasks, DashboardFilter::default());
        reversed.sort_rows(DashboardSort::AttentionOldest);
        assert_eq!(snapshot, reversed);
    }

    #[test]
    fn derived_views_match_full_projection_and_keep_search_scope_counts() {
        let mut tasks = vec![
            task(TaskStatus::Created),
            task(TaskStatus::Working),
            task(TaskStatus::WaitingForApproval),
            task(TaskStatus::WaitingForHuman),
            task(TaskStatus::ReadyForReview),
            task(TaskStatus::Failed),
            task(TaskStatus::ReadyForReview),
            task(TaskStatus::Completed),
            task(TaskStatus::Archived),
        ];
        tasks[4].validation.status = TaskValidationStatus::Failed;
        tasks[8].validation.status = TaskValidationStatus::Failed;
        let original = tasks.clone();
        for query in ["", "provider:codex -status:archived", "status:invalid"] {
            let scope = project(
                &tasks,
                DashboardFilter {
                    text: query,
                    ..Default::default()
                },
            );
            let scope_before = scope.clone();
            let attention_counts = scope.attention_counts();
            if !query.contains("invalid") {
                for kind in DashboardAttentionKind::ALL {
                    assert_eq!(attention_counts.get(kind), 1);
                }
            }
            for view in [
                DashboardView::All,
                DashboardView::NeedsAttention,
                DashboardView::Running,
                DashboardView::NotStarted,
                DashboardView::Review,
                DashboardView::Finished,
            ] {
                for attention in std::iter::once(None).chain(DashboardAttentionKind::ALL.map(Some))
                {
                    for sort in DashboardSort::ALL {
                        let mut expected = project(
                            &tasks,
                            DashboardFilter {
                                text: query,
                                view,
                                ..Default::default()
                            },
                        );
                        expected.rows.retain(|row| {
                            attention.is_none_or(|kind| row.attention_kind() == Some(kind))
                        });
                        expected.sort_rows(sort);
                        assert_eq!(scope.view(view, attention, sort), expected);
                        let mut in_place = scope.clone();
                        in_place.apply_view(view, attention, sort);
                        assert_eq!(in_place, expected);
                    }
                }
            }
            assert_eq!(scope, scope_before);
            assert_eq!(attention_counts, scope.attention_counts());
            assert_eq!(
                DashboardAttentionKind::ALL
                    .iter()
                    .map(|kind| attention_counts.get(*kind))
                    .sum::<usize>(),
                scope.counts.attention
            );
        }
        assert_eq!(tasks, original);
    }

    #[test]
    fn unsorted_scope_preserves_borrowed_candidate_order_and_projects_activity_once() {
        let tasks = [
            task(TaskStatus::Working),
            task(TaskStatus::WaitingForApproval),
            task(TaskStatus::Created),
        ];
        let visits = std::cell::Cell::new(0);
        let scope = DashboardSnapshot::project_unsorted(
            &tasks,
            |_| {
                visits.set(visits.get() + 1);
                DashboardActivity::default()
            },
            DashboardFilter::default(),
        );
        assert_eq!(visits.get(), tasks.len());
        assert_eq!(
            scope.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            tasks.iter().map(|task| task.id).collect::<Vec<_>>()
        );
        let borrowed = DashboardSnapshot::filter_iter_unsorted(
            scope.rows.iter(),
            "provider:codex",
            DashboardView::All,
        );
        assert_eq!(borrowed, scope);
        let derived = borrowed.view(DashboardView::NeedsAttention, None, DashboardSort::Recent);
        assert_eq!(derived.rows.len(), 1);
        assert_eq!(derived.rows[0].id, tasks[1].id);
        assert_eq!(derived.counts.total, 3);
        assert_eq!(
            visits.get(),
            tasks.len(),
            "view switches never resolve live activity again"
        );
    }

    #[test]
    fn attention_navigation_moves_both_ways_through_only_visible_tasks() {
        let mut tasks = vec![
            task(TaskStatus::Working),
            task(TaskStatus::WaitingForApproval),
            task(TaskStatus::Completed),
            task(TaskStatus::WaitingForHuman),
        ];
        for (index, task) in tasks.iter_mut().enumerate() {
            task.title = index.to_string();
        }
        let mut snapshot = project(&tasks, DashboardFilter::default());
        snapshot.sort_rows(DashboardSort::Title);
        for (selected, next, previous) in [
            (None, 1, 3),
            (Some(tasks[0].id), 1, 3),
            (Some(tasks[1].id), 3, 3),
            (Some(tasks[2].id), 3, 1),
            (Some(tasks[3].id), 1, 1),
            (Some(TaskId::new()), 1, 3),
        ] {
            assert_eq!(snapshot.next_attention(selected), Some(tasks[next].id));
            assert_eq!(
                snapshot.previous_attention(selected),
                Some(tasks[previous].id)
            );
        }
        snapshot.rows.retain(|row| row.id != tasks[3].id);
        assert_eq!(
            snapshot.previous_attention(Some(tasks[3].id)),
            Some(tasks[1].id)
        );
        assert_eq!(
            snapshot.previous_attention(Some(tasks[1].id)),
            Some(tasks[1].id)
        );
        snapshot.rows.retain(|row| !row.needs_attention);
        assert_eq!(snapshot.previous_attention(None), None);
        assert_eq!(snapshot.next_attention(None), None);
        assert_eq!(DashboardSnapshot::default().previous_attention(None), None);
    }

    #[test]
    fn task_search_terms_span_fields_and_can_find_exact_task_ids() {
        let task = task(TaskStatus::Created);
        let tasks = [task.clone()];
        assert_eq!(
            project(
                &tasks,
                DashboardFilter {
                    text: "dashboard codex repo",
                    ..Default::default()
                }
            )
            .rows
            .len(),
            1
        );
        assert_eq!(
            project(
                &tasks,
                DashboardFilter {
                    text: &task.id.to_string(),
                    ..Default::default()
                }
            )
            .rows[0]
                .id,
            task.id
        );
        assert!(project(
            &tasks,
            DashboardFilter {
                text: "dashboard missing",
                ..Default::default()
            }
        )
        .rows
        .is_empty());
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
    fn attention_queues_partition_only_actionable_rows() {
        for status in [
            TaskStatus::Created,
            TaskStatus::Starting,
            TaskStatus::Working,
            TaskStatus::WaitingForApproval,
            TaskStatus::WaitingForHuman,
            TaskStatus::ReadyForReview,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
            TaskStatus::Archived,
        ] {
            for validation in [
                TaskValidationStatus::NotRun,
                TaskValidationStatus::Running,
                TaskValidationStatus::Passed,
                TaskValidationStatus::Failed,
                TaskValidationStatus::Inconclusive,
                TaskValidationStatus::Cancelled,
            ] {
                let mut task = task(status);
                task.validation.status = validation;
                let before = task.clone();
                let row = DashboardRow::from_task(&task, DashboardActivity::default());
                let kind = row.attention_kind();
                assert_eq!(
                    kind.is_some(),
                    row.needs_attention,
                    "{status:?} / {validation:?}"
                );
                if let Some(kind) = kind {
                    assert!(DashboardAttentionKind::ALL.contains(&kind));
                    if status == TaskStatus::WaitingForApproval {
                        assert_eq!(kind, DashboardAttentionKind::Approval);
                    } else if status == TaskStatus::WaitingForHuman {
                        assert_eq!(kind, DashboardAttentionKind::Input);
                    } else if status == TaskStatus::Failed {
                        assert_eq!(kind, DashboardAttentionKind::Failed);
                    } else if validation.needs_attention() {
                        assert_eq!(kind, DashboardAttentionKind::Validation);
                    }
                }
                assert_eq!(task, before);
            }
        }
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
