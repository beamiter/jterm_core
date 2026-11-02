//! One bounded query language for the unified sidebar and document search.
//! Lexing and task-state aliases reuse the task dashboard. Project paths use
//! the session library's lexical directory-and-descendant semantics. No query
//! performs filesystem I/O or changes runtime state.

use super::{ConversationKind, RuntimeState, WorkspaceAttention, WorkspaceFilter, WorkspaceRow};
use crate::agent_sessions::SessionStatus;
use crate::agent_task::{query as task_query, TaskStatus, TaskValidationStatus};
use std::path::{Component, Path, PathBuf};

pub const WORKSPACE_QUERY_MAX_BYTES: usize = 4096;
const MAX_TERMS: usize = 64;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WorkspaceSort {
    #[default]
    Relevance,
    Recent,
    Oldest,
    PinnedFirst,
    NeedsAttention,
}

impl WorkspaceSort {
    pub fn label(self) -> &'static str {
        match self {
            Self::Relevance => "Best match",
            Self::Recent => "Recent first",
            Self::Oldest => "Oldest first",
            Self::PinnedFirst => "Pinned first",
            Self::NeedsAttention => "Needs you first",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Field {
    Text,
    Body,
    Title,
    Project,
    Branch,
    Id,
    Provider,
    Kind,
    Tag,
    Status,
    Runtime,
    Validation,
    Attention,
    Is,
}

impl Field {
    fn parse(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "title" | "name" => Self::Title,
            "project" | "cwd" | "repo" | "workspace" => Self::Project,
            "branch" => Self::Branch,
            "id" => Self::Id,
            "provider" => Self::Provider,
            "kind" | "type" => Self::Kind,
            "tag" => Self::Tag,
            "status" | "state" => Self::Status,
            "runtime" => Self::Runtime,
            "validation" => Self::Validation,
            "attention" => Self::Attention,
            "is" => Self::Is,
            "body" | "content" => Self::Body,
            _ => return None,
        })
    }

    fn alternatives(self) -> bool {
        matches!(
            self,
            Self::Provider
                | Self::Kind
                | Self::Status
                | Self::Runtime
                | Self::Validation
                | Self::Attention
                | Self::Is
        )
    }

    fn accepts(self, value: &str) -> bool {
        match self {
            Self::Kind => matches!(
                value,
                "api" | "chat" | "cli" | "terminal" | "task" | "native"
            ),
            Self::Status => {
                task_query::status(value).is_some()
                    || matches!(
                        value,
                        "waiting" | "idle" | "unknown" | "running" | "stopping" | "interrupted"
                    )
            }
            Self::Runtime => task_query::runtime(value).is_some() || runtime_state(value).is_some(),
            Self::Validation => task_query::validation(value).is_some(),
            Self::Attention => attention(value).is_some() || value == "none",
            Self::Is => matches!(
                value,
                "live"
                    | "running"
                    | "pinned"
                    | "archived"
                    | "attention"
                    | "needs-you"
                    | "interrupted"
                    | "preparing"
                    | "agent-stream"
                    | "validation-running"
                    | "not-started"
                    | "review"
                    | "finished"
            ),
            _ => !value.is_empty(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Term {
    field: Field,
    values: Vec<String>,
    absolute_project: Option<PathBuf>,
    negated: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceQuery {
    terms: Vec<Term>,
}

impl WorkspaceQuery {
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > WORKSPACE_QUERY_MAX_BYTES {
            return Err(format!(
                "Workspace search is limited to {WORKSPACE_QUERY_MAX_BYTES} bytes."
            ));
        }
        if text.chars().any(|character| {
            !matches!(character, '\t' | '\n' | '\r')
                && (character.is_control()
                    || crate::review_input::is_visual_spoofing_character(character))
        }) {
            return Err(
                "Remove hidden control or direction-changing characters from the search.".into(),
            );
        }
        let mut terms = Vec::new();
        for token in task_query::tokenize_with_limit(text.trim(), MAX_TERMS)? {
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
                    (Field::Text, token.value.as_str())
                } else {
                    return Err(format!("Unknown workspace filter '{name}'. Use title:, project:, branch:, id:, provider:, kind:, tag:, status:, runtime:, validation:, attention:, is: or body:. Quote literal text containing a colon."));
                }
            } else {
                (Field::Text, token.value.as_str())
            };
            let absolute_project = (field == Field::Project)
                .then(|| normalized_absolute(Path::new(value)))
                .flatten();
            let values: Vec<_> = if field.alternatives() {
                value.split(',').map(task_query::normalize).collect()
            } else {
                vec![value.to_lowercase()]
            };
            if values
                .iter()
                .any(|value| value.is_empty() || !field.accepts(value))
            {
                return Err(format!(
                    "Invalid or empty workspace filter '{}'.",
                    token.value
                ));
            }
            terms.push(Term {
                field,
                values,
                absolute_project,
                negated: token.negated,
            });
        }
        Ok(Self { terms })
    }

    pub fn matches(&self, row: &WorkspaceRow) -> bool {
        self.matches_text(row, &RowText::new(row), None)
    }

    /// Free words may match metadata or the document body. Field constraints
    /// retain their meaning, including exclusions. An SQL/FTS candidate filter
    /// must not discard metadata-only terms before this residual predicate.
    pub fn matches_document(&self, row: &WorkspaceRow, body: &str) -> bool {
        self.matches_text(row, &RowText::new(row), Some(&body.to_lowercase()))
    }

    /// Positive, unfielded terms only. Returned text is normalized but is not
    /// SQL syntax, FTS syntax, shell text or an authorization predicate.
    pub fn plain_terms(&self) -> Vec<&str> {
        self.terms
            .iter()
            .filter(|term| term.field == Field::Text && !term.negated)
            .flat_map(|term| term.values.iter().map(String::as_str))
            .collect()
    }

    pub fn includes_archived(&self) -> bool {
        self.terms.iter().any(|term| {
            !term.negated
                && matches!(term.field, Field::Is | Field::Status)
                && term.values.iter().any(|value| value == "archived")
        })
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    fn matches_text(&self, row: &WorkspaceRow, text: &RowText, body: Option<&str>) -> bool {
        self.terms.iter().all(|term| {
            let matched = term.values.iter().any(|value| match term.field {
                Field::Text => {
                    text.fields().any(|field| field.contains(value.as_str()))
                        || body.is_some_and(|body| body.contains(value.as_str()))
                }
                Field::Body => body.is_some_and(|body| body.contains(value.as_str())),
                Field::Title => text.title.contains(value.as_str()),
                Field::Project => match &term.absolute_project {
                    Some(path) => text
                        .directory
                        .as_ref()
                        .is_some_and(|directory| directory.starts_with(path)),
                    None => text.cwd.contains(value.as_str()),
                },
                Field::Branch => text.branch.contains(value.as_str()),
                Field::Id => text.id.contains(value.as_str()),
                Field::Provider => {
                    text.provider == *value || (value == "open-code" && text.provider == "opencode")
                }
                Field::Kind => match value.as_str() {
                    "api" | "chat" => row.kind == ConversationKind::Api,
                    "cli" | "terminal" => row.kind == ConversationKind::Cli,
                    "task" | "native" => row.kind == ConversationKind::Task,
                    _ => false,
                },
                Field::Tag => text.tags.iter().any(|tag| tag == value),
                Field::Status => matches_status(row, value),
                Field::Runtime => {
                    runtime_state(value) == Some(row.runtime)
                        || (row.facts.task_runtime.is_some()
                            && task_query::runtime(value) == row.facts.task_runtime)
                }
                Field::Validation => {
                    row.facts.validation_status.is_some()
                        && task_query::validation(value) == row.facts.validation_status
                }
                Field::Attention => {
                    if value == "none" {
                        row.attention().is_none()
                    } else {
                        attention(value).is_some_and(|value| row.attention() == Some(value))
                    }
                }
                Field::Is => match value.as_str() {
                    "live" | "running" => row.runtime.is_live(),
                    "pinned" => row.pinned,
                    "archived" => row.archived,
                    "attention" | "needs-you" => row.attention().is_some(),
                    "interrupted" => row.facts.interrupted,
                    "preparing" => row.facts.preparing || row.runtime == RuntimeState::Starting,
                    "agent-stream" => row.facts.agent_stream,
                    "validation-running" => {
                        row.facts.validation_status == Some(TaskValidationStatus::Running)
                    }
                    "not-started" => row.facts.task_status == Some(TaskStatus::Created),
                    "review" => row.facts.task_status == Some(TaskStatus::ReadyForReview),
                    "finished" => {
                        matches!(
                            row.facts.task_status,
                            Some(
                                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
                            )
                        ) || matches!(
                            row.facts.api_state,
                            Some(
                                super::ApiState::Completed
                                    | super::ApiState::Cancelled
                                    | super::ApiState::Failed
                            )
                        ) || row.runtime == RuntimeState::Exited
                    }
                    _ => false,
                },
            });
            matched != term.negated
        })
    }

    fn score(&self, text: &RowText) -> u64 {
        self.terms
            .iter()
            .filter(|term| !term.negated)
            .map(|term| {
                term.values
                    .iter()
                    .map(|value| {
                        if matches!(term.field, Field::Id | Field::Text)
                            && (text.id == *value || text.native_id.as_deref() == Some(value))
                        {
                            1_000_000
                        } else if matches!(term.field, Field::Title | Field::Text)
                            && text.title == *value
                        {
                            100_000
                        } else if matches!(term.field, Field::Title | Field::Text)
                            && text.title.starts_with(value.as_str())
                        {
                            10_000
                        } else if matches!(term.field, Field::Title | Field::Text)
                            && text.title.contains(value.as_str())
                        {
                            1_000
                        } else if term.field == Field::Text
                            && (text.cwd.contains(value.as_str())
                                || text.branch.contains(value.as_str()))
                        {
                            100
                        } else {
                            0
                        }
                    })
                    .max()
                    .unwrap_or(0)
            })
            .sum()
    }
}

struct RowText {
    id: String,
    native_id: Option<String>,
    title: String,
    provider: String,
    cwd: String,
    directory: Option<PathBuf>,
    branch: String,
    preview: String,
    tags: Vec<String>,
}

impl RowText {
    fn new(row: &WorkspaceRow) -> Self {
        let native_id = row
            .id
            .0
            .strip_prefix("jterm:cli:v1:")
            .and_then(|value| serde_json::from_str::<(String, String, String, String)>(value).ok())
            .map(|(_, _, _, id)| id.to_lowercase());
        Self {
            id: row.id.0.to_lowercase(),
            native_id,
            title: row.title.to_lowercase(),
            provider: row.provider.to_lowercase(),
            cwd: row.cwd.to_lowercase(),
            directory: normalized_absolute(Path::new(&row.cwd)),
            branch: row
                .facts
                .branch
                .as_deref()
                .unwrap_or_default()
                .to_lowercase(),
            preview: row.preview.to_lowercase(),
            tags: row.tags.iter().map(|tag| tag.to_lowercase()).collect(),
        }
    }
    fn fields(&self) -> impl Iterator<Item = &str> {
        [
            &self.id,
            &self.title,
            &self.provider,
            &self.cwd,
            &self.branch,
            &self.preview,
        ]
        .into_iter()
        .map(String::as_str)
        .chain(self.tags.iter().map(String::as_str))
    }
}

/// Cached metadata projection. Rebuild only after facts/organization change;
/// typing and sorting then allocate no lowercase copies per row.
pub struct WorkspaceSearchIndex {
    rows: Vec<WorkspaceRow>,
    text: Vec<RowText>,
}

impl WorkspaceSearchIndex {
    pub fn new(rows: &[WorkspaceRow]) -> Self {
        Self {
            rows: rows.to_vec(),
            text: rows.iter().map(RowText::new).collect(),
        }
    }
    pub fn rows(&self) -> &[WorkspaceRow] {
        &self.rows
    }
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn query(&self, query: &WorkspaceQuery, sort: WorkspaceSort) -> Vec<usize> {
        let mut matches: Vec<_> = self
            .rows
            .iter()
            .zip(&self.text)
            .enumerate()
            .filter(|(_, (row, text))| query.matches_text(row, text, None))
            .map(|(index, (_, text))| (index, query.score(text)))
            .collect();
        matches.sort_by(|(ai, ar), (bi, br)| {
            let a = &self.rows[*ai];
            let b = &self.rows[*bi];
            let order = match sort {
                WorkspaceSort::Relevance if !query.is_empty() => br.cmp(ar),
                WorkspaceSort::Relevance | WorkspaceSort::PinnedFirst => b.pinned.cmp(&a.pinned),
                WorkspaceSort::NeedsAttention => {
                    b.attention().is_some().cmp(&a.attention().is_some())
                }
                WorkspaceSort::Oldest => a.updated_at_ms.cmp(&b.updated_at_ms),
                WorkspaceSort::Recent => b.updated_at_ms.cmp(&a.updated_at_ms),
            };
            order
                .then(b.updated_at_ms.cmp(&a.updated_at_ms))
                .then(a.id.cmp(&b.id))
        });
        matches.into_iter().map(|(index, _)| index).collect()
    }

    pub fn project(
        &self,
        filter: &WorkspaceFilter,
        sort: WorkspaceSort,
    ) -> Result<Vec<WorkspaceRow>, String> {
        let query = WorkspaceQuery::parse(&filter.query)?;
        Ok(self
            .query(&query, sort)
            .into_iter()
            .map(|index| &self.rows[index])
            .filter(|row| {
                (filter.include_archived || query.includes_archived() || !row.archived)
                    && filter.kind.is_none_or(|kind| row.kind == kind)
                    && (!filter.live_only || row.runtime.is_live())
            })
            .cloned()
            .collect())
    }
}

fn attention(value: &str) -> Option<WorkspaceAttention> {
    if value == "reply" {
        return Some(WorkspaceAttention::Reply);
    }
    WorkspaceAttention::ALL
        .into_iter()
        .find(|kind| kind.query_value() == value)
}

fn runtime_state(value: &str) -> Option<RuntimeState> {
    Some(match value {
        "idle" => RuntimeState::Idle,
        "starting" => RuntimeState::Starting,
        "running" | "detached" => RuntimeState::Running,
        "attached" => RuntimeState::Attached,
        "stopping" => RuntimeState::Stopping,
        "exited" => RuntimeState::Exited,
        "failed" => RuntimeState::Failed,
        "conflict" => RuntimeState::Conflict,
        _ => return None,
    })
}

fn matches_status(row: &WorkspaceRow, value: &str) -> bool {
    if let Some(status) = row.facts.task_status {
        return task_query::status(value) == Some(status);
    }
    if let Some(state) = row.facts.api_state {
        return match state {
            super::ApiState::Starting => value == "starting",
            super::ApiState::Running => matches!(value, "running" | "working"),
            super::ApiState::Stopping => value == "stopping",
            super::ApiState::Completed => matches!(value, "completed" | "done" | "idle"),
            super::ApiState::Cancelled => matches!(value, "cancelled" | "canceled"),
            super::ApiState::Failed => value == "failed",
            super::ApiState::Interrupted => value == "interrupted",
        };
    }
    matches!(
        (row.facts.recorded_status, value),
        (Some(SessionStatus::Working), "working")
            | (
                Some(SessionStatus::Waiting),
                "waiting" | "input" | "waiting-for-human"
            )
            | (Some(SessionStatus::Idle), "idle")
            | (Some(SessionStatus::Unknown), "unknown")
            | (Some(SessionStatus::Archived), "archived")
    )
}

pub(super) fn normalized_absolute(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_workspace::{ConversationId, WorkspaceFacts};

    fn row(id: &str, title: &str, cwd: &str) -> WorkspaceRow {
        WorkspaceRow {
            id: ConversationId(id.into()),
            kind: ConversationKind::Cli,
            title: title.into(),
            provider: "Codex".into(),
            cwd: cwd.into(),
            preview: "中文日志 renderer".into(),
            updated_at_ms: 1,
            archived: false,
            pinned: false,
            tags: vec!["review".into()],
            runtime: RuntimeState::Idle,
            facts: WorkspaceFacts {
                branch: Some("feature/sidebar".into()),
                recorded_status: Some(SessionStatus::Waiting),
                ..WorkspaceFacts::default()
            },
        }
    }

    #[test]
    fn fields_phrases_exclusions_and_path_boundaries_intersect() {
        let value = row("exact-id", "Renderer redesign", "/work/repo/sub");
        let query = WorkspaceQuery::parse("provider:codex,claude title:\"renderer redesign\" tag:review project:/work/repo -branch:broken 中文").unwrap();
        assert!(query.matches(&value));
        assert!(!query.matches(&row("other", "Renderer redesign", "/work/repo-other")));
        assert!(WorkspaceQuery::parse("status:waiting -is:live")
            .unwrap()
            .matches(&value));
        assert!(!WorkspaceQuery::parse("attention:approval")
            .unwrap()
            .matches(&value));
    }

    #[test]
    fn malformed_and_unknown_constraints_fail_closed_without_widening_scope() {
        let rows = vec![row("one", "Anything", "/work/repo")];
        for text in [
            "title:",
            "status:imaginary",
            "attention:thought",
            "title:\"unfinished",
            "-",
            "provider:codex,",
            "\u{202e}foo",
        ] {
            let filter = WorkspaceFilter {
                query: text.into(),
                ..WorkspaceFilter::default()
            };
            assert!(filter.query_error().is_some(), "{text}");
            assert!(filter.project(&rows).is_empty(), "{text}");
        }
    }

    #[test]
    fn full_text_terms_can_span_metadata_and_body_including_negation() {
        let value = row("one", "Renderer redesign", "/work/repo");
        let query = WorkspaceQuery::parse("provider:codex renderer 零复制 -panic").unwrap();
        assert!(!query.matches(&value));
        assert!(query.matches_document(&value, "实现零复制读取"));
        assert!(!query.matches_document(&value, "零复制 panic"));
        assert_eq!(query.plain_terms(), vec!["renderer", "零复制"]);
        assert!(WorkspaceQuery::parse("body:零复制")
            .unwrap()
            .matches_document(&value, "零复制"));
    }

    #[test]
    fn quoted_colons_urls_and_windows_paths_remain_literal() {
        let mut value = row(
            "one",
            "status:failed https://example.test C:\\work",
            "/work/repo",
        );
        assert!(
            WorkspaceQuery::parse("\"status:failed\" https://example.test C:\\work")
                .unwrap()
                .matches(&value)
        );
        value.title = "-literal".into();
        assert!(WorkspaceQuery::parse("\"-literal\"")
            .unwrap()
            .matches(&value));
    }

    #[test]
    fn cached_ranking_preserves_exact_identity_and_explicit_sort_meaning() {
        let mut exact = row("target", "Old", "/work/repo");
        exact.updated_at_ms = 1;
        let mut recent = row("other", "target", "/work/repo");
        recent.updated_at_ms = 100;
        recent.pinned = true;
        let index = WorkspaceSearchIndex::new(&[recent, exact]);
        let query = WorkspaceQuery::parse("target").unwrap();
        assert_eq!(index.query(&query, WorkspaceSort::Relevance), vec![1, 0]);
        assert_eq!(index.query(&query, WorkspaceSort::Recent), vec![0, 1]);
        assert_eq!(
            index.query(&WorkspaceQuery::default(), WorkspaceSort::Relevance),
            vec![0, 1]
        );
    }

    #[test]
    fn archived_constraint_can_open_archive_but_never_override_other_scope() {
        let mut value = row("one", "Archived", "/work/repo");
        value.archived = true;
        let rows = [value];
        let mut filter = WorkspaceFilter::default();
        assert!(filter.project(&rows).is_empty());
        filter.query = "is:archived".into();
        assert_eq!(filter.project(&rows).len(), 1);
        filter.kind = Some(ConversationKind::Api);
        assert!(filter.project(&rows).is_empty());
    }

    #[test]
    fn task_constraints_match_existing_dashboard_across_lifecycle_and_validation() {
        use crate::agent_task::{
            AgentProvider, DashboardActivity, DashboardQuery, DashboardRow, NewTask, TaskManager,
            TaskRuntimeKind,
        };
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
        let mut task = manager.get(id).unwrap().clone();
        task.runtime_kind = TaskRuntimeKind::Native;
        let queries = [
            "provider:codex,claude -status:completed",
            "status:review",
            "status:waiting_for_approval",
            "validation:failed,inconclusive",
            "attention:approval",
            "attention:validation",
            "attention:review",
            "runtime:native",
            "is:review",
            "project:\"team repo\" branch:agent-input title:\"agent input\" 保留草稿",
        ];
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
            task.status = status;
            for validation in [
                TaskValidationStatus::NotRun,
                TaskValidationStatus::Running,
                TaskValidationStatus::Passed,
                TaskValidationStatus::Failed,
                TaskValidationStatus::Inconclusive,
                TaskValidationStatus::Cancelled,
            ] {
                task.validation.status = validation;
                let activity = DashboardActivity::default();
                let old = DashboardRow::from_task(&task, activity);
                let unified = WorkspaceRow::from_task("fixture", &task, activity).unwrap();
                for text in queries {
                    assert_eq!(
                        WorkspaceQuery::parse(text).unwrap().matches(&unified),
                        DashboardQuery::parse(text).unwrap().matches(&old),
                        "{status:?} {validation:?} {text}"
                    );
                }
            }
        }
    }
}
