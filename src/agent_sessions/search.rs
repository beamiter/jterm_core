//! Cached, deterministic session search shared by every native frontend.
//! Build once after a changed catalog; queries never inspect the filesystem.
use super::{AgentSession, SessionCatalog, SessionLibrarySnapshot, SessionProvider, SessionStatus};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

const MAX_QUERY_BYTES: usize = 4096;
const MAX_QUERY_TERMS: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionSort {
    #[default]
    Relevance,
    Recent,
    LiveFirst,
    RecentlyResumed,
}
impl SessionSort {
    pub const ALL: [Self; 4] = [
        Self::Relevance,
        Self::Recent,
        Self::LiveFirst,
        Self::RecentlyResumed,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Relevance => "Best match",
            Self::Recent => "Recent",
            Self::LiveFirst => "Live first",
            Self::RecentlyResumed => "Recently resumed",
        }
    }
}

impl std::fmt::Display for SessionSort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionLibraryFilter {
    pub pinned_only: bool,
    pub recent_only: bool,
    /// Empty-query default browsing only; never displace exact search matches.
    pub pinned_first: bool,
}

/// `project` includes the absolute directory and its lexical descendants.
/// `/work/repo` never includes `/work/repo-other`. Symlink aliases remain
/// distinct; query evaluation performs no canonicalization or other disk I/O.
#[derive(Clone, Copy, Debug, Default)]
pub struct SessionQuery<'a> {
    pub text: &'a str,
    pub provider: Option<SessionProvider>,
    pub status: Option<SessionStatus>,
    pub live_only: bool,
    pub project: Option<&'a Path>,
    pub sort: SessionSort,
}

#[derive(Clone, Debug, Default)]
pub struct SessionSearchIndex {
    entries: Vec<SearchEntry>,
}
#[derive(Clone, Debug)]
struct SearchEntry {
    provider: SessionProvider,
    status: SessionStatus,
    id: String,
    key: String,
    title: String,
    cwd: String,
    branch: String,
    preview: String,
    provider_name: String,
    activity: &'static str,
    directory: Option<PathBuf>,
    updated_at_ms: u64,
    live: bool,
}
impl SessionSearchIndex {
    /// Normalize searchable metadata once per changed discovery snapshot.
    pub fn new(catalog: &SessionCatalog) -> Self {
        let live = catalog
            .live_processes
            .iter()
            .map(|process| (process.provider, process.id.as_str()))
            .collect::<HashSet<_>>();
        Self {
            entries: catalog
                .sessions
                .iter()
                .map(|session| {
                    SearchEntry::new(
                        session,
                        live.contains(&(session.provider, session.id.as_str())),
                    )
                })
                .collect(),
        }
    }
    pub fn query_with_library(
        &self,
        query: &SessionQuery<'_>,
        library: &SessionLibrarySnapshot,
        filter: SessionLibraryFilter,
    ) -> Vec<usize> {
        let mut indices = self.query(query);
        indices.retain(|index| {
            let entry = &self.entries[*index];
            (!filter.pinned_only || library.is_pinned(entry.provider, &entry.key))
                && (!filter.recent_only
                    || library.last_resumed(entry.provider, &entry.key).is_some())
        });
        let pinned_first = filter.pinned_first
            && query.text.trim().is_empty()
            && query.sort == SessionSort::Relevance;
        if pinned_first || query.sort == SessionSort::RecentlyResumed {
            // Stable sort preserves relevance/recency and identity tie-breaks
            // from the base projection when preference metadata ties.
            indices.sort_by(|a, b| {
                let left = &self.entries[*a];
                let right = &self.entries[*b];
                let pins = if pinned_first {
                    library
                        .is_pinned(right.provider, &right.key)
                        .cmp(&library.is_pinned(left.provider, &left.key))
                } else {
                    std::cmp::Ordering::Equal
                };
                pins.then_with(|| {
                    if query.sort == SessionSort::RecentlyResumed {
                        library
                            .last_resumed(right.provider, &right.key)
                            .cmp(&library.last_resumed(left.provider, &left.key))
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
            });
        }
        indices
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// All whitespace-separated terms must match some indexed field. The
    /// returned indices refer to the catalog supplied to `new`, never sorted
    /// row numbers or a refreshed catalog. Rebuild after replacing that catalog.
    pub fn query(&self, query: &SessionQuery<'_>) -> Vec<usize> {
        if query.text.len() > MAX_QUERY_BYTES {
            return Vec::new();
        }
        let normalized = query.text.trim().to_lowercase();
        let terms = normalized.split_whitespace().collect::<Vec<_>>();
        if terms.len() > MAX_QUERY_TERMS {
            return Vec::new();
        }
        let project = match query.project {
            Some(path) => match normalized_absolute(path) {
                Some(path) => Some(path),
                None => return Vec::new(),
            },
            None => None,
        };
        let mut ranked = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if query
                    .provider
                    .is_some_and(|provider| entry.provider != provider)
                    || query.status.is_some_and(|status| entry.status != status)
                    || (query.live_only && !entry.live)
                    || project.as_ref().is_some_and(|root| {
                        entry
                            .directory
                            .as_ref()
                            .is_none_or(|directory| !directory.starts_with(root))
                    })
                {
                    return None;
                }
                entry.score(&normalized, &terms).map(|score| (index, score))
            })
            .collect::<Vec<_>>();
        ranked.sort_unstable_by(|(left, left_score), (right, right_score)| {
            let a = &self.entries[*left];
            let b = &self.entries[*right];
            let priority = match query.sort {
                SessionSort::Recent | SessionSort::RecentlyResumed => std::cmp::Ordering::Equal,
                SessionSort::Relevance => right_score.cmp(left_score),
                SessionSort::LiveFirst => b
                    .live
                    .cmp(&a.live)
                    .then_with(|| right_score.cmp(left_score)),
            };
            priority
                .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
                .then_with(|| provider_order(a.provider).cmp(&provider_order(b.provider)))
                .then_with(|| a.key.cmp(&b.key))
                .then_with(|| left.cmp(right))
        });
        ranked.into_iter().map(|(index, _)| index).collect()
    }
}
impl SessionCatalog {
    /// Convenient one-shot projection. Native event loops should cache a
    /// `SessionSearchIndex` instead of rebuilding it on every rendered frame.
    pub fn query(&self, query: &SessionQuery<'_>) -> Vec<usize> {
        SessionSearchIndex::new(self).query(query)
    }
}
impl SearchEntry {
    fn new(session: &AgentSession, live: bool) -> Self {
        Self {
            provider: session.provider,
            status: session.status,
            id: session.id.to_lowercase(),
            key: session.id.clone(),
            title: session.title.trim().to_lowercase(),
            cwd: session.cwd.to_string_lossy().to_lowercase(),
            branch: session.branch.as_deref().unwrap_or("").to_lowercase(),
            preview: session.preview.to_lowercase(),
            provider_name: session.provider.label().to_lowercase(),
            activity: match session.status {
                SessionStatus::Working => "working running last recorded",
                SessionStatus::Waiting => "waiting needs input approval last recorded",
                SessionStatus::Idle => "idle ready resumable",
                SessionStatus::Unknown => "unknown state",
                SessionStatus::Archived => "archived",
            },
            directory: normalized_absolute(&session.cwd),
            updated_at_ms: session.updated_at_ms,
            live,
        }
    }
    fn score(&self, query: &str, terms: &[&str]) -> Option<u64> {
        let mut score = 0u64;
        for term in terms {
            // Tier minima exceed the next tier's maximum. Besides giving
            // titles/IDs priority, this avoids rescanning long previews when
            // a term already matched a stronger field.
            let identity =
                field_score(&self.id, term, 100_000).max(field_score(&self.title, term, 30_000));
            let value = if identity > 0 {
                identity
            } else {
                let location =
                    field_score(&self.cwd, term, 5_000).max(field_score(&self.branch, term, 5_000));
                if location > 0 {
                    location
                } else {
                    let preview = field_score(&self.preview, term, 500);
                    if preview > 0 {
                        preview
                    } else {
                        field_score(&self.provider_name, term, 100).max(field_score(
                            self.activity,
                            term,
                            100,
                        ))
                    }
                }
            };
            if value == 0 {
                return None;
            }
            score += u64::from(value);
        }
        if !query.is_empty() {
            if self.id == query {
                score += 1_000_000_000;
            }
            if self.title == query {
                score += 100_000;
            }
        }
        Some(score)
    }
}
fn field_score(field: &str, term: &str, weight: u32) -> u32 {
    if field == term {
        return weight * 4;
    }
    if field.starts_with(term) {
        return weight * 3;
    }
    let mut matched = false;
    let boundary = field.match_indices(term).any(|(index, _)| {
        matched = true;
        field[..index]
            .chars()
            .next_back()
            .is_some_and(|character| !character.is_alphanumeric())
    });
    if boundary {
        weight * 2
    } else if matched {
        weight
    } else {
        0
    }
}
fn provider_order(provider: SessionProvider) -> u8 {
    match provider {
        SessionProvider::Codex => 0,
        SessionProvider::Claude => 1,
    }
}
fn normalized_absolute(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_sessions::LiveSessionProcess;
    fn session(id: &str, title: &str, cwd: &str, preview: &str, updated: u64) -> AgentSession {
        AgentSession {
            provider: SessionProvider::Codex,
            id: id.into(),
            title: title.into(),
            cwd: cwd.into(),
            updated_at_ms: updated,
            status: SessionStatus::Idle,
            preview: preview.into(),
            branch: None,
            source: "/tmp/source.jsonl".into(),
            archived: false,
        }
    }
    fn catalog(sessions: Vec<AgentSession>) -> SessionCatalog {
        SessionCatalog {
            sessions,
            ..SessionCatalog::default()
        }
    }
    #[test]
    fn title_matches_beat_newer_project_and_preview_matches() {
        let catalog = catalog(vec![
            session(
                "preview",
                "Unrelated task",
                "/work/other",
                "repair renderer",
                300,
            ),
            session("path", "Unrelated task", "/work/repair/renderer", "", 200),
            session("title", "Repair renderer", "/work/other", "", 100),
        ]);
        let index = SessionSearchIndex::new(&catalog);
        let query = SessionQuery {
            text: "repair renderer",
            ..SessionQuery::default()
        };
        assert_eq!(index.query(&query), [2, 1, 0]);
        assert_eq!(
            index.query(&SessionQuery {
                sort: SessionSort::Recent,
                ..query
            }),
            [0, 1, 2]
        );
    }
    #[test]
    fn exact_uuid_beats_newer_mentions_in_title_or_preview() {
        let id = "11111111-1111-4111-8111-111111111111";
        let catalog = catalog(vec![
            session("other", id, "/work", id, 200),
            session(id, "Actual selected conversation", "/work", "", 1),
        ]);
        assert_eq!(
            catalog.query(&SessionQuery {
                text: id,
                ..SessionQuery::default()
            }),
            [1, 0]
        );
    }
    #[test]
    fn and_terms_can_span_title_branch_provider_and_unicode_preview() {
        let mut chosen = session(
            "chosen",
            "Renderer pipeline",
            "/work/graphics",
            "项目修复已完成",
            1,
        );
        chosen.branch = Some("feature/memory".into());
        let catalog = catalog(vec![
            chosen,
            session(
                "other",
                "Renderer pipeline",
                "/work/graphics",
                "unrelated",
                2,
            ),
        ]);
        let index = SessionSearchIndex::new(&catalog);
        assert_eq!(
            index.query(&SessionQuery {
                text: "RENDERER memory CODEX 项目",
                ..SessionQuery::default()
            }),
            [0]
        );
        assert!(index
            .query(&SessionQuery {
                text: "renderer missing",
                ..SessionQuery::default()
            })
            .is_empty());
    }
    #[test]
    fn project_scope_normalizes_components_and_excludes_prefix_siblings() {
        let catalog = catalog(vec![
            session("root", "Task", "/work/repo", "", 4),
            session("child", "Task", "/work/repo/subdir", "", 3),
            session("sibling", "Task", "/work/repo-other", "", 2),
            session("normalized", "Task", "/work/unused/../repo/./nested", "", 1),
        ]);
        let index = SessionSearchIndex::new(&catalog);
        let query = SessionQuery {
            project: Some(Path::new("/work/repo/./")),
            ..SessionQuery::default()
        };
        assert_eq!(index.query(&query), [0, 1, 3]);
        assert!(index
            .query(&SessionQuery {
                project: Some(Path::new("relative/repo")),
                ..query
            })
            .is_empty());
    }
    #[test]
    fn filters_are_composed_and_liveness_is_separate_from_recorded_state() {
        let mut a = session("alive", "Alpha", "/repo", "", 1);
        a.status = SessionStatus::Working;
        let mut b = session("claude", "Beta", "/repo", "", 2);
        b.provider = SessionProvider::Claude;
        b.status = SessionStatus::Waiting;
        let mut catalog = catalog(vec![a, b, session("idle", "Gamma", "/repo", "", 3)]);
        catalog.live_processes = vec![
            LiveSessionProcess {
                provider: SessionProvider::Codex,
                id: "alive".into(),
                pid: 42,
            },
            LiveSessionProcess {
                provider: SessionProvider::Claude,
                id: "claude".into(),
                pid: 43,
            },
        ];
        let index = SessionSearchIndex::new(&catalog);
        assert_eq!(
            index.query(&SessionQuery {
                provider: Some(SessionProvider::Codex),
                status: Some(SessionStatus::Working),
                live_only: true,
                ..SessionQuery::default()
            }),
            [0]
        );
        assert!(index
            .query(&SessionQuery {
                provider: Some(SessionProvider::Codex),
                status: Some(SessionStatus::Idle),
                live_only: true,
                ..SessionQuery::default()
            })
            .is_empty());
        assert_eq!(
            index.query(&SessionQuery {
                sort: SessionSort::LiveFirst,
                ..SessionQuery::default()
            }),
            [1, 0, 2]
        );
    }
    #[test]
    fn empty_relevance_query_is_recent_and_ties_use_stable_identity() {
        let mut b = session("b", "Beta", "/repo", "", 100);
        b.provider = SessionProvider::Claude;
        let a = session("a", "Alpha", "/repo", "", 100);
        let z = session("z", "Zulu", "/repo", "", 100);
        let mut catalog = catalog(vec![b, a, z]);
        let selected_keys = |catalog: &SessionCatalog| {
            let index = SessionSearchIndex::new(catalog);
            index
                .query(&SessionQuery::default())
                .into_iter()
                .map(|i| (catalog.sessions[i].provider, catalog.sessions[i].id.clone()))
                .collect::<Vec<_>>()
        };
        let before = selected_keys(&catalog);
        catalog.sessions.reverse();
        assert_eq!(before, selected_keys(&catalog));
        assert_eq!(
            before,
            vec![
                (SessionProvider::Codex, "a".into()),
                (SessionProvider::Codex, "z".into()),
                (SessionProvider::Claude, "b".into())
            ]
        );
        assert_eq!(
            SessionSearchIndex::new(&catalog).query(&SessionQuery::default()),
            SessionSearchIndex::new(&catalog).query(&SessionQuery {
                sort: SessionSort::Recent,
                ..SessionQuery::default()
            })
        );
    }
    #[test]
    fn cached_index_is_immutable_until_catalog_replaced_and_equality_detects_changes() {
        let mut original = catalog(vec![session("same", "Old title", "/repo", "", 1)]);
        let identical = original.clone();
        assert_eq!(original, identical);
        let before = SessionSearchIndex::new(&original);
        original.sessions[0].title = "New title".into();
        assert_ne!(original, identical);
        assert_eq!(
            before.query(&SessionQuery {
                text: "old",
                ..SessionQuery::default()
            }),
            [0]
        );
        assert!(before
            .query(&SessionQuery {
                text: "new",
                ..SessionQuery::default()
            })
            .is_empty());
        assert_eq!(
            SessionSearchIndex::new(&original).query(&SessionQuery {
                text: "new",
                ..SessionQuery::default()
            }),
            [0]
        );
    }
    #[test]
    fn word_boundary_priority_considers_later_occurrences() {
        assert!(
            field_score("concatenate cat", "cat", 100) > field_score("concatenate", "cat", 100)
        );
    }
    #[test]
    fn excessive_query_is_rejected_instead_of_ignoring_required_terms() {
        let index = SessionSearchIndex::new(&catalog(vec![session("a", "target", "/repo", "", 1)]));
        assert!(index
            .query(&SessionQuery {
                text: &"x".repeat(MAX_QUERY_BYTES + 1),
                ..SessionQuery::default()
            })
            .is_empty());
        assert!(index
            .query(&SessionQuery {
                text: &"target ".repeat(MAX_QUERY_TERMS + 1),
                ..SessionQuery::default()
            })
            .is_empty());
    }
    #[test]
    fn synthetic_2048_sessions_repeated_queries_use_the_cached_projection() {
        let sessions = (0..2048)
            .map(|i| {
                session(
                    &format!("session-{i:04}"),
                    &format!("Repair renderer {i}"),
                    &format!("/work/project-{}/src", i % 16),
                    &format!("{} 项目修复", "context ".repeat(200)),
                    i,
                )
            })
            .collect();
        let catalog = catalog(sessions);
        let started = std::time::Instant::now();
        let index = SessionSearchIndex::new(&catalog);
        let build = started.elapsed();
        let started = std::time::Instant::now();
        for _ in 0..20 {
            let result = std::hint::black_box(index.query(&SessionQuery {
                text: "renderer project-3",
                ..SessionQuery::default()
            }));
            assert_eq!(result.len(), 128);
            assert!(result
                .windows(2)
                .all(|pair| catalog.sessions[pair[0]].updated_at_ms
                    > catalog.sessions[pair[1]].updated_at_ms));
        }
        eprintln!(
            "2048-session cached index: build={build:?},20 ranked queries={:?}",
            started.elapsed()
        );
        assert_eq!(index.len(), 2048);
        assert!(!index.is_empty());
        assert!(SessionSearchIndex::default().is_empty());
    }
    #[test]
    fn library_filters_sort_receipts_and_never_override_nonempty_search() {
        use crate::agent_sessions::SessionLibraryEntry;
        let a = "11111111-1111-4111-8111-111111111111";
        let b = "22222222-2222-4222-8222-222222222222";
        let c = "33333333-3333-4333-8333-333333333333";
        let catalog = catalog(vec![
            session(a, "Exact renderer", "/repo", "", 300),
            session(b, "Other work", "/repo", "renderer", 200),
            session(c, "Never resumed", "/repo", "", 100),
        ]);
        let library = SessionLibrarySnapshot::from_entries(vec![
            SessionLibraryEntry {
                provider: SessionProvider::Codex,
                id: a.into(),
                pinned: false,
                last_resumed_ms: Some(10),
            },
            SessionLibraryEntry {
                provider: SessionProvider::Codex,
                id: b.into(),
                pinned: true,
                last_resumed_ms: Some(20),
            },
        ])
        .unwrap();
        let index = SessionSearchIndex::new(&catalog);
        assert_eq!(
            index.query_with_library(
                &SessionQuery::default(),
                &library,
                SessionLibraryFilter {
                    pinned_first: true,
                    ..SessionLibraryFilter::default()
                }
            ),
            [1, 0, 2]
        );
        assert_eq!(
            index.query_with_library(
                &SessionQuery {
                    text: "renderer",
                    ..SessionQuery::default()
                },
                &library,
                SessionLibraryFilter {
                    pinned_first: true,
                    ..SessionLibraryFilter::default()
                }
            ),
            [0, 1]
        );
        assert_eq!(
            index.query_with_library(
                &SessionQuery {
                    sort: SessionSort::RecentlyResumed,
                    ..SessionQuery::default()
                },
                &library,
                SessionLibraryFilter::default()
            ),
            [1, 0, 2]
        );
        assert_eq!(
            index.query_with_library(
                &SessionQuery::default(),
                &library,
                SessionLibraryFilter {
                    pinned_only: true,
                    recent_only: true,
                    pinned_first: false
                }
            ),
            [1]
        );
    }
}
