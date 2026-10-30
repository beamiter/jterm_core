//! Native rich-conversation component model and explicit visual-acceptance data.
//!
//! The fixture is opt-in, clearly labelled and never represents a running task.
//! Frontends render these same component types from observed runtime data when
//! available. They must not turn terminal output into invented agent progress.

pub const FIXTURE_ENV: &str = "JTERM_REFERENCE_FIXTURE";
pub const FIXTURE_LABEL: &str = "Visual acceptance fixture · sample data";
pub const FIXTURE_DATE_SEPARATOR: &str = "Today · 14:32 · sample";
pub const FIXTURE_TERMINAL_EXPANDED: bool = true;
pub const METADATA_ICON_KEYS: [&str; 6] =
    ["folder", "terminal", "code", "history", "agent", "check"];

pub fn fixture_enabled() -> bool {
    std::env::var_os(FIXTURE_ENV).as_deref() == Some(std::ffi::OsStr::new("1"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Shell,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepState {
    Complete,
    Running,
    Queued,
    /// An observed request, review, or stopped result requiring attention.
    Attention,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub title: String,
    pub detail: String,
    pub state: StepState,
    pub elapsed: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub kind: DiffKind,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeDiff {
    pub path: String,
    pub language: String,
    pub added: usize,
    pub removed: usize,
    pub lines: Vec<DiffLine>,
    /// False until a real edit target and its authorization exist.
    pub can_apply: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Heading(String),
    Paragraph(String),
    Metadata(Vec<String>),
    Steps(Vec<Step>),
    Diff(CodeDiff),
    Terminal { title: String, text: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub author: String,
    pub avatar: String,
    pub timing: Option<String>,
    pub parts: Vec<Part>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inspector {
    pub title: String,
    pub subtitle: String,
    pub badge: String,
    pub completed: usize,
    pub total: usize,
    pub metadata: Vec<(String, String)>,
    pub current_step: Option<Step>,
    pub queue: Vec<Step>,
    pub output_title: String,
    pub output: String,
}

impl Inspector {
    /// Complete component architecture with an honest neutral production state.
    pub fn empty() -> Self {
        Self {
            title: "Agent workspace".into(),
            subtitle: "Start or select a task".into(),
            badge: "No task".into(),
            completed: 0,
            total: 0,
            metadata: vec![
                ("Model".into(), "Not selected".into()),
                ("Workspace".into(), "—".into()),
                ("Branch".into(), "—".into()),
                ("Environment".into(), "—".into()),
                ("Started".into(), "—".into()),
            ],
            current_step: None,
            queue: vec![],
            output_title: "Terminal output".into(),
            output: "No task output".into(),
        }
    }
}

/// Resolve exactly the task shown by [`observed_task_inspector`]. Frontends
/// also use this ID for their existing task-navigation actions. Hidden tasks
/// cannot remain selected through a stale sidebar selection.
pub fn observed_inspector_task(
    tasks: &[crate::agent_task::AgentTask],
    selected: Option<crate::agent_task::TaskId>,
) -> Option<&crate::agent_task::AgentTask> {
    use crate::agent_task::TaskStatus;
    selected
        .and_then(|id| tasks.iter().find(|task| task.id == id))
        .filter(|task| task.status != TaskStatus::Archived)
        .or_else(|| {
            tasks
                .iter()
                .filter(|task| task.status != TaskStatus::Archived)
                .max_by_key(|task| inspector_task_priority(task))
        })
}

fn inspector_task_priority(task: &crate::agent_task::AgentTask) -> (u8, u64, u64) {
    use crate::agent_task::{TaskStatus, TaskValidationStatus};
    let priority = match task.status {
        TaskStatus::WaitingForApproval => 6,
        TaskStatus::WaitingForHuman => 5,
        _ if task.validation.status == TaskValidationStatus::Running => 4,
        TaskStatus::Starting | TaskStatus::Working => 4,
        TaskStatus::ReadyForReview => 3,
        TaskStatus::Failed => 2,
        TaskStatus::Created => 1,
        TaskStatus::Completed | TaskStatus::Cancelled | TaskStatus::Archived => 0,
    };
    (priority, task.updated_at_ms, task.created_at_ms)
}

fn observed_task_step_state(task: &crate::agent_task::AgentTask) -> StepState {
    use crate::agent_task::{TaskStatus, TaskValidationStatus};
    match task.status {
        TaskStatus::Completed => StepState::Complete,
        TaskStatus::WaitingForApproval
        | TaskStatus::WaitingForHuman
        | TaskStatus::ReadyForReview
        | TaskStatus::Failed
        | TaskStatus::Cancelled
        | TaskStatus::Archived => {
            if task.status == TaskStatus::ReadyForReview
                && task.validation.status == TaskValidationStatus::Running
            {
                StepState::Running
            } else {
                StepState::Attention
            }
        }
        TaskStatus::Starting | TaskStatus::Working => StepState::Running,
        TaskStatus::Created => {
            if task.brief.is_some() || task.source_context.is_some() {
                StepState::Queued
            } else {
                StepState::Attention
            }
        }
    }
}

/// Bounded read-only projection of recorded tasks into the common inspector.
/// Counts mean accepted/visible tasks, not inferred agent steps. Runtime
/// activity only refines guidance; it grants no action or execution authority.
/// Output is recorded task detail, never an unrelated terminal transcript.
/// Adapters apply their workspace privacy/redaction policy before rendering.
pub fn observed_task_inspector(
    tasks: &[crate::agent_task::AgentTask],
    selected: Option<crate::agent_task::TaskId>,
    activity: impl Fn(crate::agent_task::TaskId) -> crate::agent_task::DashboardActivity,
) -> Inspector {
    use crate::agent_task::{TaskGuidance, TaskInspector, TaskStatus};
    use crate::review_input::{safe_inline_display, safe_multiline_display};

    let Some(task) = observed_inspector_task(tasks, selected) else {
        return Inspector::empty();
    };
    let mut visible: Vec<_> = tasks
        .iter()
        .enumerate()
        .filter(|(_, task)| task.status != TaskStatus::Archived)
        .collect();
    visible.sort_by_key(|(index, task)| std::cmp::Reverse((inspector_task_priority(task), *index)));
    let facts = TaskInspector::from_task(task);
    let guidance = TaskGuidance::from_task(task, activity(task.id));
    let mut metadata = Vec::new();
    for (source, label) in [
        ("Provider", "Provider"),
        ("Repository", "Workspace"),
        ("Branch", "Branch"),
        ("Runtime", "Environment"),
        ("Validation", "Validation"),
        ("Created (UTC)", "Created (UTC)"),
        ("Updated (UTC)", "Updated (UTC)"),
    ] {
        if let Some(field) = facts.fields.iter().find(|field| field.label == source) {
            metadata.push((label.into(), field.value.clone()));
        }
    }
    let mut detail = guidance.detail.to_owned();
    if let Some(reason) = guidance.reason {
        detail.push_str("\n\n");
        detail.push_str(&reason);
    }
    let mut output = Vec::new();
    if let Some(goal) = facts.goal {
        output.push(format!("Goal\n{goal}"));
    }
    for (label, value) in [
        ("Latest status", task.status_detail.as_deref()),
        ("Validation", task.validation.status_detail.as_deref()),
    ] {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            output.push(format!("{label}\n{}", safe_multiline_display(value, 2048)));
        }
    }
    if let Some(code) = task.exit_code {
        output.push(format!("Agent exit: {code}"));
    }
    if let Some(code) = task.validation.exit_code {
        output.push(format!("Validation exit: {code}"));
    }
    if output.is_empty() {
        output.push("Open this task to inspect its worktree, terminal or review details.".into());
    }
    let completed = visible
        .iter()
        .filter(|(_, task)| task.status == TaskStatus::Completed)
        .count();
    let total = visible.len();
    Inspector {
        title: safe_inline_display(&task.title, 256),
        subtitle: format!("{completed} / {total} tasks accepted"),
        badge: task.status.label().into(),
        completed,
        total,
        metadata,
        current_step: Some(Step {
            title: guidance.title.into(),
            detail: safe_multiline_display(&detail, 4096),
            state: observed_task_step_state(task),
            elapsed: None,
        }),
        queue: visible
            .into_iter()
            .filter(|(_, other)| other.id != task.id)
            .take(8)
            .map(|(_, task)| Step {
                title: safe_inline_display(&task.title, 256),
                detail: format!("{} · {}", task.provider.display_name(), task.status.label()),
                state: observed_task_step_state(task),
                elapsed: None,
            })
            .collect(),
        output_title: "Task details".into(),
        output: safe_multiline_display(&output.join("\n\n"), 8192),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    pub is_fixture: bool,
    pub title: String,
    pub subtitle: String,
    pub messages: Vec<Message>,
    pub inspector: Inspector,
}

/// One sample for all four native renderers, never a source of execution state.
pub fn acceptance_fixture() -> Conversation {
    let steps = vec![
        Step {
            title: "Inspect the current renderer".into(),
            detail: "Reading layout and scroll ownership".into(),
            state: StepState::Complete,
            elapsed: Some("0.4s".into()),
        },
        Step {
            title: "Keep the reading anchor stable".into(),
            detail: "Retain fractional viewport position".into(),
            state: StepState::Complete,
            elapsed: Some("0.8s".into()),
        },
        Step {
            title: "Update the canvas components".into(),
            detail: "Compose the native message surface".into(),
            state: StepState::Running,
            elapsed: Some("Fixture".into()),
        },
        Step {
            title: "Review narrow-window layout".into(),
            detail: "Queued sample state".into(),
            state: StepState::Queued,
            elapsed: None,
        },
        Step {
            title: "Compare reference geometry".into(),
            detail: "Queued sample state".into(),
            state: StepState::Queued,
            elapsed: None,
        },
    ];
    let diff = CodeDiff {
        path: "src/canvas.rs".into(),
        language: "Rust".into(),
        added: 3,
        removed: 1,
        can_apply: false,
        lines: vec![
            DiffLine {
                old_line: Some(38),
                new_line: Some(38),
                kind: DiffKind::Context,
                text: "// Viewport positions use logical pixels.".into(),
            },
            DiffLine {
                old_line: Some(39),
                new_line: Some(39),
                kind: DiffKind::Context,
                text: "#[inline]".into(),
            },
            DiffLine {
                old_line: Some(40),
                new_line: Some(40),
                kind: DiffKind::Context,
                text: "fn viewport_offset(row: f64) -> f64 {".into(),
            },
            DiffLine {
                old_line: Some(41),
                new_line: None,
                kind: DiffKind::Removed,
                text: "    row.floor()".into(),
            },
            DiffLine {
                old_line: None,
                new_line: Some(41),
                kind: DiffKind::Added,
                text: "    // Keep the reader's fractional position.".into(),
            },
            DiffLine {
                old_line: None,
                new_line: Some(42),
                kind: DiffKind::Added,
                text: "    let anchor = row;".into(),
            },
            DiffLine {
                old_line: None,
                new_line: Some(43),
                kind: DiffKind::Added,
                text: "    anchor".into(),
            },
            DiffLine {
                old_line: Some(42),
                new_line: Some(44),
                kind: DiffKind::Context,
                text: "}".into(),
            },
            DiffLine {
                old_line: Some(43),
                new_line: Some(45),
                kind: DiffKind::Context,
                text: String::new(),
            },
        ],
    };
    Conversation {
        is_fixture: true,
        title: "Workspace / Canvas renderer".into(), subtitle: FIXTURE_LABEL.into(),
        messages: vec![
            Message { role: Role::User, author: "You".into(), avatar: "Y".into(), timing: None,
                parts: vec![Part::Paragraph("Update the canvas renderer with retained scroll and an infinite canvas.".into())] },
            Message { role: Role::Assistant, author: "Assistant".into(), avatar: "✦".into(), timing: Some("Sample response · 2.1s".into()),
                parts: vec![
                    Part::Heading("One continuous canvas, a stable reading position".into()),
                    Part::Paragraph("The message surface keeps your place as new output arrives. Navigation, context and tools remain together around the canvas.".into()),
                    Part::Metadata(vec!["Read files".into(), "Canvas renderer".into(), "3 files".into(), "Review diff".into(), "Native tools".into(), "2 / 5 steps".into()]),
                    Part::Steps(steps[..4].to_vec()),
                    Part::Diff(diff),
                    Part::Terminal { title: "Terminal · sample output".into(), text: "$ inspect canvas\nReading layout contract…\nRetained viewport ready for visual review.\n# Fixture text; no process was executed.".into() },
                ] },
        ],
        inspector: Inspector {
            title: "Canvas renderer".into(), subtitle: "Sample native component state".into(),
            badge: "Running · fixture".into(), completed: 2, total: 5,
            metadata: vec![("Model".into(), "Not connected".into()), ("Workspace".into(), "canvas-workspace".into()),
                ("Branch".into(), "main · sample".into()), ("Environment".into(), "Local · fixture".into()),
                ("Started".into(), "14:32 · sample".into())],
            current_step: Some(steps[2].clone()), queue: steps,
            output_title: "Terminal output · fixture".into(),
            output: "# Visual acceptance fixture\nReading the canvas contract…\nPreparing native components…\nAwaiting visual review.\nFixture data does not execute commands.".into(),
        },
    }
}

/// Shared navigation order. Destinations bind to each frontend's real actions.
pub const NAVIGATION: [(&str, &str); 5] = [
    ("conversation", "Chat"),
    ("folder", "Files & Code"),
    ("terminal", "Shell"),
    ("agent", "Agent Tasks"),
    ("settings", "Settings"),
];
pub const SPACES: [(&str, &str); 4] = [
    ("Anvil", "Terminal workspace"),
    ("Forge", "Project sessions"),
    ("Ember", "Creative workspace"),
    ("Frost", "Project workspace"),
];

/// Shared rail order; Settings and the local profile remain anchored below.
pub const RAIL_NAVIGATION: [(&str, &str); 5] = [
    ("workspace", "Workspace"),
    ("folder", "Files & Code"),
    ("terminal", "Shell"),
    ("agent", "Agent Tasks"),
    ("search", "Search"),
];
pub const COMPOSER_TOOL_NAMES: [&str; 5] = ["Attach", "Image", "Code", "Terminal", "Tools"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentEntry {
    pub title: String,
    pub detail: String,
    pub time: String,
    pub icon_key: &'static str,
    pub selected: bool,
}

/// History rows for the explicit sample scene, never inserted in real history.
pub fn fixture_recent_entries() -> Vec<RecentEntry> {
    [
        ("Canvas renderer", "Retained scroll", "Now", "terminal"),
        (
            "Review workspace layout",
            "Native components",
            "12m",
            "conversation",
        ),
        ("Project context", "Files and code", "1h", "files"),
        (
            "Terminal session",
            "Local workspace",
            "Yesterday",
            "terminal",
        ),
        ("Keyboard focus", "Native input", "2d", "agent"),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (title, detail, time, icon_key))| RecentEntry {
        title: title.into(),
        detail: detail.into(),
        time: time.into(),
        icon_key,
        selected: index == 0,
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::{
        AgentProvider, AgentTask, DashboardActivity, NewTask, TaskBrief, TaskManager, TaskStatus,
        TaskValidationStatus,
    };

    fn task(title: &str, status: TaskStatus, updated: u64) -> AgentTask {
        let mut manager = TaskManager::new();
        let id = manager
            .create_with_brief(
                NewTask {
                    title: title.into(),
                    provider: AgentProvider::Codex,
                    repo_root: "/workspace/project".into(),
                    worktree_path: "/workspace/worktree".into(),
                    branch: "task/inspector".into(),
                    base_commit: "a".repeat(40),
                    source_context: None,
                },
                TaskBrief::new("Implement the requested panel", None).unwrap(),
            )
            .unwrap();
        let mut task = manager.get(id).unwrap().clone();
        task.status = status;
        task.created_at_ms = 1;
        task.updated_at_ms = updated;
        task
    }

    #[test]
    fn observed_empty_or_hidden_tasks_do_not_create_activity() {
        assert_eq!(
            observed_task_inspector(&[], None, |_| DashboardActivity::default()),
            Inspector::empty()
        );
        let hidden = task("Hidden", TaskStatus::Archived, 100);
        assert!(observed_inspector_task(std::slice::from_ref(&hidden), Some(hidden.id)).is_none());
        assert_eq!(
            observed_task_inspector(&[hidden], None, |_| panic!("no active task")),
            Inspector::empty()
        );
    }

    #[test]
    fn observed_task_selection_and_navigation_share_one_stable_identity() {
        let tasks = vec![
            task("Selected", TaskStatus::Created, 1),
            task("Working", TaskStatus::Working, 50),
            task("Needs approval", TaskStatus::WaitingForApproval, 30),
            task("Hidden", TaskStatus::Archived, 100),
        ];
        for (selected, expected) in [
            (Some(tasks[0].id), 0),
            (Some(tasks[1].id), 1),
            (Some(tasks[3].id), 2),
            (None, 2),
        ] {
            let resolved = observed_inspector_task(&tasks, selected).unwrap();
            let view = observed_task_inspector(&tasks, selected, |id| {
                assert_eq!(id, resolved.id);
                DashboardActivity::default()
            });
            assert_eq!(resolved.id, tasks[expected].id);
            assert_eq!(view.title, tasks[expected].title);
            assert!(!view.queue.iter().any(|step| step.title == view.title));
            assert!(!view.queue.iter().any(|step| step.title == "Hidden"));
        }
        let newest = vec![
            task("Older", TaskStatus::Working, 30),
            task("Newer", TaskStatus::Working, 60),
        ];
        assert_eq!(
            observed_inspector_task(&newest, None).unwrap().id,
            newest[1].id
        );
    }

    #[test]
    fn observed_statuses_do_not_turn_review_failure_or_questions_into_queued_work() {
        for (status, state) in [
            (TaskStatus::Created, StepState::Queued),
            (TaskStatus::Starting, StepState::Running),
            (TaskStatus::Working, StepState::Running),
            (TaskStatus::WaitingForApproval, StepState::Attention),
            (TaskStatus::WaitingForHuman, StepState::Attention),
            (TaskStatus::ReadyForReview, StepState::Attention),
            (TaskStatus::Failed, StepState::Attention),
            (TaskStatus::Cancelled, StepState::Attention),
            (TaskStatus::Completed, StepState::Complete),
        ] {
            let tasks = vec![task("Observed", status, 1)];
            let view = observed_task_inspector(&tasks, None, |_| DashboardActivity::default());
            assert_eq!(view.badge, status.label());
            assert_eq!(view.current_step.unwrap().state, state);
            assert_eq!(view.total, 1);
            assert_eq!(view.completed, usize::from(status == TaskStatus::Completed));
        }
        let mut recovered = task("Recovered", TaskStatus::Created, 1);
        recovered.brief = None;
        let view = observed_task_inspector(&[recovered], None, |_| DashboardActivity::default());
        let step = view.current_step.unwrap();
        assert_eq!(step.state, StepState::Attention);
        assert_eq!(step.title, "Task instructions are missing");
    }

    #[test]
    fn observed_guidance_tracks_actual_validation_and_live_streams_without_acceptance() {
        let mut current = task("Review", TaskStatus::ReadyForReview, 1);
        current.validation.status = TaskValidationStatus::Running;
        let tasks = vec![current];
        let before = tasks.clone();
        let view = observed_task_inspector(&tasks, None, |_| DashboardActivity::default());
        let step = view.current_step.unwrap();
        assert_eq!(step.title, "Validation is running");
        assert_eq!(step.state, StepState::Running);
        assert_eq!(view.completed, 0);
        assert_eq!(tasks, before);
        let mut current = tasks[0].clone();
        current.validation.status = TaskValidationStatus::Passed;
        let view = observed_task_inspector(&[current], None, |_| DashboardActivity {
            has_active_agent_stream: true,
            ..Default::default()
        });
        assert_eq!(
            view.current_step.unwrap().title,
            "Review or continue the live session"
        );
        assert_eq!(
            view.completed, 0,
            "a validation pass does not accept a task"
        );
    }

    #[test]
    fn observed_task_details_are_bounded_and_not_an_unrelated_terminal_transcript() {
        let mut current = task("Observed", TaskStatus::Failed, 1);
        current.title = format!("\u{1b}[2J\u{202e}{}", "任务".repeat(500));
        current.status_detail = Some(format!("\u{1b}[2J\u{202e}{}", "错误\n".repeat(4000)));
        current.validation.status = TaskValidationStatus::Failed;
        current.validation.status_detail = Some("Actual validation failure".into());
        current.exit_code = Some(9);
        current.validation.exit_code = Some(1);
        let view = observed_task_inspector(&[current], None, |_| DashboardActivity::default());
        assert!(view.title.len() <= 256);
        assert!(view.output.len() <= 8192);
        assert!(!view.title.contains(['\u{1b}', '\u{202e}']));
        assert!(!view.output.contains(['\u{1b}', '\u{202e}']));
        assert!(view.output.contains("Agent exit: 9"));
        assert!(view.output.contains("Validation exit: 1"));
        assert!(view.output.contains("Actual validation failure"));
        assert_eq!(view.output_title, "Task details");
        assert!(!view.metadata.iter().any(|(key, _)| key == "Model"));
    }

    #[test]
    fn observed_queue_is_bounded_but_acceptance_counts_cover_all_visible_tasks() {
        let mut tasks: Vec<_> = (0..20)
            .map(|index| task(&format!("Task {index}"), TaskStatus::Created, index))
            .collect();
        tasks[0].status = TaskStatus::Completed;
        tasks[1].status = TaskStatus::ReadyForReview;
        tasks[2].status = TaskStatus::Failed;
        tasks[3].status = TaskStatus::Cancelled;
        tasks[4].status = TaskStatus::Archived;
        let view =
            observed_task_inspector(&tasks, Some(tasks[1].id), |_| DashboardActivity::default());
        assert_eq!(view.completed, 1);
        assert_eq!(view.total, 19);
        assert_eq!(view.queue.len(), 8);
        assert!(view.subtitle.contains("tasks accepted"));
        assert!(!view
            .queue
            .iter()
            .any(|step| step.title == "Task 1" || step.title == "Task 4"));
    }
    #[test]
    fn fixture_cannot_be_mistaken_for_a_connected_or_applicable_task() {
        let fixture = acceptance_fixture();
        assert!(fixture.is_fixture);
        assert_eq!(fixture.subtitle, FIXTURE_LABEL);
        assert_eq!(fixture.inspector.badge, "Running · fixture");
        assert!(fixture
            .inspector
            .metadata
            .iter()
            .any(|(_, value)| value == "Not connected"));
        for message in fixture.messages {
            for part in message.parts {
                if let Part::Diff(diff) = part {
                    assert!(!diff.can_apply);
                }
            }
        }
    }
}
