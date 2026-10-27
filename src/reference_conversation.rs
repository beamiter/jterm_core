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
