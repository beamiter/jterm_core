//! Bounded, read-only task queries shared by every dashboard. A malformed
//! constraint never silently broadens a query, and matching performs no I/O.

use super::dashboard::{DashboardAttentionKind, DashboardRow, DASHBOARD_QUERY_MAX_BYTES};
use super::{AgentProvider, TaskRuntimeKind, TaskStatus, TaskValidationStatus};

const MAX_TERMS: usize = 32;

/// All terms are ANDed. A leading `-` negates one term; double quotes preserve
/// a phrase. Enumerated fields accept comma-separated alternatives (OR).
/// Text fields such as `project:` are case-insensitive display-text searches,
/// never filesystem identity checks or authorization predicates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DashboardQuery {
    terms: Vec<Term>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Text,
    Title,
    Project,
    Branch,
    Id,
    Provider,
    Status,
    Runtime,
    Validation,
    Attention,
    Is,
}

impl Field {
    fn parse(value: &str) -> Option<Self> {
        Some(match value.to_ascii_lowercase().as_str() {
            "title" => Self::Title,
            "project" | "repo" | "workspace" => Self::Project,
            "branch" => Self::Branch,
            "id" => Self::Id,
            "provider" => Self::Provider,
            "status" | "state" => Self::Status,
            "runtime" => Self::Runtime,
            "validation" => Self::Validation,
            "attention" => Self::Attention,
            "is" => Self::Is,
            _ => return None,
        })
    }

    fn is_enumerated(self) -> bool {
        matches!(
            self,
            Self::Provider
                | Self::Status
                | Self::Runtime
                | Self::Validation
                | Self::Attention
                | Self::Is
        )
    }

    fn accepts(self, value: &str) -> bool {
        match self {
            Self::Provider => provider(value).is_some(),
            Self::Status => status(value).is_some(),
            Self::Runtime => runtime(value).is_some(),
            Self::Validation => validation(value).is_some(),
            Self::Attention => attention(value).is_some(),
            Self::Is => matches!(
                value,
                "attention"
                    | "running"
                    | "not-started"
                    | "review"
                    | "finished"
                    | "archived"
                    | "preparing"
                    | "agent-stream"
                    | "validation-running"
            ),
            _ => !value.is_empty(),
        }
    }

    fn help(self) -> &'static str {
        match self {
            Self::Provider => "provider:codex,claude,opencode,kimi",
            Self::Status => "status:created,starting,working,approval,input,review,completed,failed,cancelled,archived",
            Self::Runtime => "runtime:unassigned,terminal,native,fallback",
            Self::Validation => "validation:not-run,running,passed,failed,inconclusive,cancelled",
            Self::Attention => "attention:approval,input,validation,review,failed",
            Self::Is => "is:attention,running,not-started,review,finished,archived,preparing,agent-stream,validation-running",
            _ => "Use a non-empty search value; put phrases in double quotes.",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Term {
    field: Field,
    values: Vec<String>,
    negated: bool,
}

struct Token {
    value: String,
    negated: bool,
    // A fully quoted term such as "status:failed" is literal text.
    literal: bool,
}

impl DashboardQuery {
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > DASHBOARD_QUERY_MAX_BYTES {
            return Err(format!(
                "Task search is limited to {DASHBOARD_QUERY_MAX_BYTES} bytes."
            ));
        }
        if text.chars().any(|character| {
            (character.is_control() && !matches!(character, '\t' | '\n' | '\r'))
                || crate::review_input::is_visual_spoofing_character(character)
        }) {
            return Err(
                "Remove hidden control or direction-changing characters from the search.".into(),
            );
        }
        let text = text.trim();
        let mut terms = Vec::new();
        for token in tokenize(text)? {
            let (field, value) = if token.literal {
                (Field::Text, token.value.as_str())
            } else if let Some((name, value)) = token.value.split_once(':') {
                if let Some(field) = Field::parse(name) {
                    (field, value)
                } else if value.starts_with("//")
                    || (name.len() == 1
                        && name.as_bytes()[0].is_ascii_alphabetic()
                        && (value.starts_with('/') || value.starts_with('\\')))
                {
                    // URLs and Windows paths remain ordinary searchable text.
                    (Field::Text, token.value.as_str())
                } else {
                    return Err(format!(
                        "Unknown task filter '{name}'. Use provider:, status:, runtime:, validation:, attention:, is:, project:, branch:, title: or id:. Quote literal text containing a colon."
                    ));
                }
            } else {
                (Field::Text, token.value.as_str())
            };
            let values = if field.is_enumerated() {
                value.split(',').map(normalize).collect::<Vec<_>>()
            } else {
                vec![value.to_lowercase()]
            };
            if values
                .iter()
                .any(|value| value.is_empty() || !field.accepts(value))
            {
                return Err(format!("Invalid or empty task filter. {}", field.help()));
            }
            terms.push(Term {
                field,
                values,
                negated: token.negated,
            });
        }
        Ok(Self { terms })
    }

    pub fn matches(&self, row: &DashboardRow) -> bool {
        if self.terms.is_empty() {
            return true;
        }
        // Normalize displayed fields once per row, rather than once per term.
        let title = row.title.to_lowercase();
        let project = row.repo_label.to_lowercase();
        let branch = row.branch.to_lowercase();
        let id = row.id.to_string();
        let detail = row
            .status_detail
            .as_deref()
            .unwrap_or_default()
            .to_lowercase();
        let provider_label = row.provider.display_name().to_lowercase();
        let state = row.status.label().to_lowercase();
        let runtime_label = row.runtime_kind.label().to_lowercase();
        let validation_label = row.validation_status.label().to_lowercase();
        let text_fields = [
            &title,
            &project,
            &branch,
            &id,
            &detail,
            &provider_label,
            &state,
            &runtime_label,
            &validation_label,
        ];
        self.terms.iter().all(|term| {
            let matched = term.values.iter().any(|value| match term.field {
                Field::Text => text_fields.iter().any(|field| field.contains(value)),
                Field::Title => title.contains(value),
                Field::Project => project.contains(value),
                Field::Branch => branch.contains(value),
                Field::Id => id.contains(value),
                Field::Provider => provider(value) == Some(row.provider),
                Field::Status => status(value) == Some(row.status),
                Field::Runtime => runtime(value) == Some(row.runtime_kind),
                Field::Validation => validation(value) == Some(row.validation_status),
                Field::Attention => {
                    attention(value).is_some_and(|kind| row.attention_kind() == Some(kind))
                }
                Field::Is => match value.as_str() {
                    "attention" => row.needs_attention && row.status != TaskStatus::Archived,
                    "running" => row.is_running(),
                    "not-started" => row.status == TaskStatus::Created,
                    "review" => row.status == TaskStatus::ReadyForReview,
                    "finished" => matches!(
                        row.status,
                        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
                    ),
                    "archived" => row.status == TaskStatus::Archived,
                    "preparing" => row.activity.native_preparing,
                    "agent-stream" => row.activity.has_active_agent_stream,
                    "validation-running" => row.validation_status == TaskValidationStatus::Running,
                    _ => false,
                },
            });
            matched != term.negated
        })
    }
}

/// Frontends display this alongside the search field. Snapshot projection
/// returns no rows for the same invalid input, so the error cannot widen scope.
pub fn dashboard_query_error(text: &str) -> Option<String> {
    DashboardQuery::parse(text).err()
}

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character.is_whitespace() {
            continue;
        }
        let negated = character == '-';
        let mut value = String::new();
        let mut quoted = character == '"';
        let mut literal = quoted;
        if !negated && !quoted {
            value.push(character);
        }
        while let Some(&next) = chars.peek() {
            if next.is_whitespace() && !quoted {
                break;
            }
            chars.next();
            if next == '"' {
                if value.is_empty() && !quoted {
                    literal = true;
                }
                quoted = !quoted;
            } else if quoted && next == '\\' && matches!(chars.peek(), Some('"' | '\\')) {
                value.push(chars.next().expect("peeked escape character"));
            } else {
                value.push(next);
            }
        }
        if quoted {
            return Err("Close the double quote to finish the search phrase.".into());
        }
        if value.is_empty() {
            return Err("Enter a search term after '-' or inside the double quotes.".into());
        }
        tokens.push(Token {
            value,
            negated,
            literal,
        });
        if tokens.len() > MAX_TERMS {
            return Err(format!("Use at most {MAX_TERMS} task search terms."));
        }
    }
    Ok(tokens)
}

fn normalize(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if matches!(character, '_' | ' ') {
                '-'
            } else {
                character
            }
        })
        .collect()
}

fn status(value: &str) -> Option<TaskStatus> {
    Some(match value {
        "created" | "not-started" => TaskStatus::Created,
        "starting" => TaskStatus::Starting,
        "working" => TaskStatus::Working,
        "approval" | "waiting-for-approval" => TaskStatus::WaitingForApproval,
        "input" | "waiting-for-human" | "waiting-for-you" => TaskStatus::WaitingForHuman,
        "review" | "ready-for-review" => TaskStatus::ReadyForReview,
        "completed" | "done" => TaskStatus::Completed,
        "failed" => TaskStatus::Failed,
        "cancelled" | "canceled" => TaskStatus::Cancelled,
        "archived" => TaskStatus::Archived,
        _ => return None,
    })
}

fn provider(value: &str) -> Option<AgentProvider> {
    if value == "open-code" {
        Some(AgentProvider::OpenCode)
    } else {
        AgentProvider::from_config_value(value)
    }
}

fn runtime(value: &str) -> Option<TaskRuntimeKind> {
    Some(match value {
        "unassigned" | "not-started" => TaskRuntimeKind::Unassigned,
        "terminal" | "pty" => TaskRuntimeKind::Terminal,
        "native" => TaskRuntimeKind::Native,
        "fallback" | "terminal-fallback" => TaskRuntimeKind::TerminalFallback,
        _ => return None,
    })
}

fn validation(value: &str) -> Option<TaskValidationStatus> {
    Some(match value {
        "not-run" | "none" => TaskValidationStatus::NotRun,
        "running" => TaskValidationStatus::Running,
        "passed" => TaskValidationStatus::Passed,
        "failed" => TaskValidationStatus::Failed,
        "inconclusive" | "needs-review" => TaskValidationStatus::Inconclusive,
        "cancelled" | "canceled" => TaskValidationStatus::Cancelled,
        _ => return None,
    })
}

fn attention(value: &str) -> Option<DashboardAttentionKind> {
    DashboardAttentionKind::ALL
        .into_iter()
        .find(|kind| kind.query_value() == value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::{
        DashboardActivity, DashboardFilter, DashboardSnapshot, DashboardView, NewTask, TaskManager,
    };

    fn row() -> DashboardRow {
        let mut manager = TaskManager::new();
        let id = manager
            .create(NewTask {
                title: "Fix agent input 保留草稿".into(),
                provider: AgentProvider::Codex,
                repo_root: "/work/team repo".into(),
                worktree_path: "/work/task".into(),
                branch: "fix/agent-input".into(),
                base_commit: "a".repeat(40),
                source_context: None,
            })
            .unwrap();
        let mut row =
            DashboardRow::from_task(manager.get(id).unwrap(), DashboardActivity::default());
        row.status = TaskStatus::ReadyForReview;
        row.runtime_kind = TaskRuntimeKind::Native;
        row.validation_status = TaskValidationStatus::Failed;
        row.needs_attention = true;
        row
    }

    fn matches(row: &DashboardRow, text: &str) -> bool {
        DashboardQuery::parse(text).unwrap().matches(row)
    }

    #[test]
    fn structured_terms_intersect_without_changing_task_identity() {
        let row = row();
        let before = row.clone();
        assert!(matches(
            &row,
            "provider:CODEX status:review runtime:native validation:failed attention:validation"
        ));
        assert!(matches(
            &row,
            "provider:claude,codex status:working,review -validation:passed"
        ));
        assert!(matches(
            &row,
            "project:\"team repo\" branch:agent-input title:\"agent input\" 保留草稿"
        ));
        assert!(matches(&row, &format!("id:{}", &row.id.to_string()[..8])));
        assert!(!matches(&row, "provider:claude status:review"));
        assert!(!matches(&row, "-provider:codex,claude"));
        assert!(!matches(&row, "title:missing"));
        assert_eq!(row, before);
    }

    #[test]
    fn phrases_literals_and_paths_keep_their_meaning() {
        let mut row = row();
        row.title = "fix status:failed don't erase path \\\\shared https://example.test".into();
        row.repo_label = r"C:\work\team repo".into();
        assert!(matches(
            &row,
            "\"status:failed\" don't https://example.test"
        ));
        assert!(matches(&row, r"C:\work"));
        assert!(matches(&row, "project:\"C:\\work\\team repo\""));
        assert!(!matches(&row, "-\"status:failed\""));
        assert!(!matches(&row, "status:failed"));
        row.title = "a \"quoted\" word".into();
        assert!(matches(&row, r#""a \"quoted\" word""#));
        row.title = "-flag".into();
        assert!(matches(&row, "\"-flag\""));
    }

    #[test]
    fn invalid_filters_never_broaden_the_snapshot() {
        let rows = [row()];
        for text in [
            "provider:unknown",
            "provder:codex",
            "status:running",
            "status:",
            "provider:codex,",
            "is:unknown",
            "\"unfinished",
            "-",
            "\"\"",
            "branch:\"\"",
            "a\u{202e}b",
            "\u{000b}",
            "\u{000c}",
            "\u{000b}provider:codex",
            "provider:codex\u{000c}",
        ] {
            assert!(dashboard_query_error(text).is_some(), "{text}");
            let snapshot = DashboardSnapshot::filter_rows(&rows, text, DashboardView::All);
            assert!(snapshot.rows.is_empty(), "{text}");
            assert_eq!(snapshot.counts.total, 0, "{text}");
        }
        assert!(dashboard_query_error(&"x".repeat(DASHBOARD_QUERY_MAX_BYTES + 1)).is_some());
        assert!(dashboard_query_error(&"a ".repeat(MAX_TERMS + 1)).is_some());
        assert!(dashboard_query_error("保留草稿 provider:codex").is_none());
    }

    #[test]
    fn activity_validation_and_lifecycle_are_separate_filters() {
        let mut row = row();
        row.activity.has_active_agent_stream = true;
        assert!(matches(&row, "status:review is:running is:agent-stream"));
        assert!(!matches(&row, "status:working"));
        row.activity.has_active_agent_stream = false;
        row.validation_status = TaskValidationStatus::Running;
        row.needs_attention = false;
        assert!(matches(
            &row,
            "is:running is:validation-running -is:attention"
        ));
        assert!(!matches(&row, "attention:review"));
        row.validation_status = TaskValidationStatus::Passed;
        row.needs_attention = true;
        assert!(matches(&row, "attention:review -is:running"));
        assert_eq!(row.status, TaskStatus::ReadyForReview);
    }

    #[test]
    fn invalid_query_and_exact_workspace_filters_agree_on_both_projection_paths() {
        let mut manager = TaskManager::new();
        let id = manager
            .create(NewTask {
                title: "Same name".into(),
                provider: AgentProvider::Codex,
                repo_root: "/work/repo".into(),
                worktree_path: "/work/task".into(),
                branch: "task/query".into(),
                base_commit: "a".repeat(40),
                source_context: None,
            })
            .unwrap();
        let task = manager.get(id).unwrap();
        for text in [
            "",
            "provider:codex status:created",
            "status:typo",
            "project:repo",
            "-provider:codex",
        ] {
            let from_tasks = DashboardSnapshot::project(
                [task],
                |_| DashboardActivity::default(),
                DashboardFilter {
                    text,
                    ..Default::default()
                },
            );
            let row = DashboardRow::from_task(task, DashboardActivity::default());
            let from_rows = DashboardSnapshot::filter_rows(&[row], text, DashboardView::All);
            assert_eq!(from_tasks, from_rows, "{text}");
        }
        let filtered = DashboardSnapshot::project(
            [task],
            |_| DashboardActivity::default(),
            DashboardFilter {
                repo_root: Some(std::path::Path::new("/work/other")),
                text: "project:repo",
                ..Default::default()
            },
        );
        assert!(
            filtered.rows.is_empty(),
            "display text cannot widen an exact project constraint"
        );
    }
}
