//! Cached, deterministic session search shared by every native frontend.
//! Build once after a changed catalog; queries never inspect the filesystem.
use super::library::{key as library_key, SessionLibraryFacts as LibraryFacts};
use super::{AgentSession, SessionCatalog, SessionLibrarySnapshot, SessionProvider, SessionStatus};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

const MAX_QUERY_BYTES: usize = 4096;
const MAX_QUERY_TERMS: usize = 64;

/// Local organization is independent of a provider's archived transcript state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionArchiveFilter {
    #[default]
    Active,
    Archived,
    All,
}
impl SessionArchiveFilter {
    pub const ALL: [Self; 3] = [Self::Active, Self::Archived, Self::All];
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "Active library",
            Self::Archived => "Archived here",
            Self::All => "All conversations",
        }
    }
    fn matches(self, archived: bool) -> bool {
        match self {
            Self::Active => !archived,
            Self::Archived => archived,
            Self::All => true,
        }
    }
}
impl std::fmt::Display for SessionArchiveFilter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Counts include this directory and its lexical descendants, matching the
/// project picker scope. Active totals exclude local archives; archived is an
/// independent count. A recorded wait is not proof of a currently blocked CLI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionProjectSummary {
    pub root: PathBuf,
    pub total: usize,
    pub live: usize,
    pub waiting: usize,
    pub pinned: usize,
    pub archived: usize,
    pub updated_at_ms: u64,
}

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
    pub archive: SessionArchiveFilter,
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
    /// Entry positions ordered by library identity, so one merge with the
    /// ordered library maps resolves pins and organization for every row.
    library_order: Vec<usize>,
    /// Cataloged project directories in component order.
    project_roots: Vec<PathBuf>,
    /// Flattened project slots per entry: its own directory and every
    /// cataloged ancestor. `project_link_ends[i]` closes entry `i`'s range.
    project_links: Vec<u32>,
    project_link_ends: Vec<u32>,
}
#[derive(Clone, Debug)]
struct SearchEntry {
    provider: SessionProvider,
    status: SessionStatus,
    id: String,
    key: String,
    /// Prepared once so library lookups never allocate per query row.
    library_key: String,
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
        let entries = catalog
            .sessions
            .iter()
            .map(|session| {
                SearchEntry::new(
                    session,
                    live.contains(&(session.provider, session.id.as_str())),
                )
            })
            .collect::<Vec<_>>();
        let mut library_order = (0..entries.len()).collect::<Vec<_>>();
        library_order
            .sort_by(|left, right| entries[*left].library_key.cmp(&entries[*right].library_key));
        // Project membership depends only on the catalog. Resolve every
        // directory's cataloged ancestors once, so summaries need no path
        // hashing or tree walks when only library organization changes.
        let mut roots = entries
            .iter()
            .filter_map(|entry| entry.directory.as_deref())
            .collect::<Vec<_>>();
        roots.sort_unstable();
        roots.dedup();
        let slots = roots
            .iter()
            .enumerate()
            .map(|(slot, root)| (*root, slot as u32))
            .collect::<HashMap<_, _>>();
        let mut project_links = Vec::new();
        let mut project_link_ends = Vec::with_capacity(entries.len());
        for entry in &entries {
            if let Some(directory) = &entry.directory {
                project_links.extend(
                    directory
                        .ancestors()
                        .filter_map(|ancestor| slots.get(ancestor).copied()),
                );
            }
            project_link_ends.push(project_links.len() as u32);
        }
        let project_roots = roots.into_iter().map(Path::to_path_buf).collect();
        Self {
            entries,
            library_order,
            project_roots,
            project_links,
            project_link_ends,
        }
    }
    /// Library facts for every catalog row, in catalog order.
    fn library_facts<'a>(&self, library: &'a SessionLibrarySnapshot) -> Vec<LibraryFacts<'a>> {
        let mut facts = vec![LibraryFacts::default(); self.entries.len()];
        if !library.is_empty() {
            library.resolve_sorted(
                self.library_order
                    .iter()
                    .map(|&index| (index, self.entries[index].library_key.as_str())),
                &mut facts,
            );
        }
        facts
    }
    pub fn query_with_library(
        &self,
        query: &SessionQuery<'_>,
        library: &SessionLibrarySnapshot,
        filter: SessionLibraryFilter,
    ) -> Vec<usize> {
        let mut ranked = self.query_filtered(query, Some(library), filter);
        let pinned_first = filter.pinned_first
            && query.text.trim().is_empty()
            && query.sort == SessionSort::Relevance;
        let resumed_first = query.sort == SessionSort::RecentlyResumed;
        if pinned_first || resumed_first {
            // Stable sort preserves relevance/recency and identity tie-breaks
            // from the base projection when preference metadata ties. Library
            // facts were resolved once per row while filtering.
            ranked.sort_by(|left, right| {
                let pins = if pinned_first {
                    right.pinned.cmp(&left.pinned)
                } else {
                    std::cmp::Ordering::Equal
                };
                pins.then_with(|| {
                    if resumed_first {
                        right.last_resumed_ms.cmp(&left.last_resumed_ms)
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
            });
        }
        ranked.into_iter().map(|row| row.index).collect()
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn query_error(text: &str) -> Option<String> {
        parse_query(text).err()
    }
    pub fn projects(&self, library: &SessionLibrarySnapshot) -> Vec<SessionProjectSummary> {
        // Only cataloged project directories are exposed; each row counts
        // toward its own directory and every cataloged ancestor, in the
        // precomputed component order.
        let mut projects = self
            .project_roots
            .iter()
            .map(|root| SessionProjectSummary {
                root: root.clone(),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let facts = self.library_facts(library);
        let mut start = 0;
        for ((entry, facts), &end) in self.entries.iter().zip(&facts).zip(&self.project_link_ends) {
            let end = end as usize;
            for &slot in &self.project_links[start..end] {
                let project = &mut projects[slot as usize];
                project.updated_at_ms = project.updated_at_ms.max(entry.updated_at_ms);
                if facts.archived {
                    project.archived += 1;
                } else {
                    project.total += 1;
                    project.live += usize::from(entry.live);
                    project.waiting += usize::from(entry.status == SessionStatus::Waiting);
                    project.pinned += usize::from(facts.pinned);
                }
            }
            start = end;
        }
        projects
    }
    /// All whitespace-separated terms must match some indexed field. The
    /// returned indices refer to the catalog supplied to `new`, never sorted
    /// row numbers or a refreshed catalog. Rebuild after replacing that catalog.
    pub fn query(&self, query: &SessionQuery<'_>) -> Vec<usize> {
        self.query_filtered(
            query,
            None,
            SessionLibraryFilter {
                archive: SessionArchiveFilter::All,
                ..Default::default()
            },
        )
        .into_iter()
        .map(|row| row.index)
        .collect()
    }
    fn query_filtered(
        &self,
        query: &SessionQuery<'_>,
        library: Option<&SessionLibrarySnapshot>,
        filter: SessionLibraryFilter,
    ) -> Vec<RankedRow> {
        let Ok(terms) = parse_query(query.text) else {
            return Vec::new();
        };
        let normalized = query.text.trim().to_lowercase();
        // An explicit positive is:archived searches the archive even when the
        // initial browsing view is Active. Other filters still intersect.
        let archive = if filter.archive == SessionArchiveFilter::Active
            && terms.iter().any(|term| {
                term.field == SearchField::Is && term.value == "archived" && !term.negative
            }) {
            SessionArchiveFilter::Archived
        } else {
            filter.archive
        };
        let project = match query.project {
            Some(path) => match normalized_absolute(path) {
                Some(path) => Some(path),
                None => return Vec::new(),
            },
            None => None,
        };
        let facts = library.map(|library| self.library_facts(library));
        let mut ranked = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                // Catalog-only constraints are checked before library facts.
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
                // Without a library every organization fact is absent.
                let facts = facts
                    .as_ref()
                    .map_or_else(LibraryFacts::default, |facts| facts[index]);
                if !archive.matches(facts.archived)
                    || (filter.pinned_only && !facts.pinned)
                    || (filter.recent_only && facts.last_resumed_ms.is_none())
                {
                    return None;
                }
                entry
                    .score_terms(&normalized, &terms, &facts)
                    .map(|score| RankedRow {
                        index,
                        score,
                        pinned: facts.pinned,
                        last_resumed_ms: facts.last_resumed_ms,
                    })
            })
            .collect::<Vec<_>>();
        ranked.sort_unstable_by(|left, right| {
            let a = &self.entries[left.index];
            let b = &self.entries[right.index];
            let priority = match query.sort {
                SessionSort::Recent | SessionSort::RecentlyResumed => std::cmp::Ordering::Equal,
                SessionSort::Relevance => right.score.cmp(&left.score),
                SessionSort::LiveFirst => b
                    .live
                    .cmp(&a.live)
                    .then_with(|| right.score.cmp(&left.score)),
            };
            priority
                .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
                .then_with(|| provider_order(a.provider).cmp(&provider_order(b.provider)))
                .then_with(|| a.key.cmp(&b.key))
                .then_with(|| left.index.cmp(&right.index))
        });
        ranked
    }
}

/// One matching catalog row with the library facts its final order may need.
#[derive(Clone, Copy, Debug)]
struct RankedRow {
    index: usize,
    score: u64,
    pinned: bool,
    last_resumed_ms: Option<u64>,
}
impl SessionCatalog {
    /// Convenient one-shot projection. Native event loops should cache a
    /// `SessionSearchIndex` instead of rebuilding it on every rendered frame.
    pub fn query(&self, query: &SessionQuery<'_>) -> Vec<usize> {
        SessionSearchIndex::new(self).query(query)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchField {
    Any,
    Title,
    Id,
    Provider,
    Project,
    Branch,
    Tag,
    Status,
    Is,
}
#[derive(Debug)]
struct SearchTerm {
    field: SearchField,
    value: String,
    path: Option<PathBuf>,
    negative: bool,
}

/// A small bounded query language, not a shell or regex. Unknown prefixes stay
/// literal (URLs and paths remain searchable). Quotes keep a phrase together;
/// a leading minus outside quotes excludes a term.
fn parse_query(text: &str) -> Result<Vec<SearchTerm>, String> {
    if text.len() > MAX_QUERY_BYTES {
        return Err("Search is limited to 4,096 bytes.".into());
    }
    if crate::review_input::contains_noncontrol_visual_spoofing(text)
        || text.chars().any(|c| c.is_control() && !c.is_whitespace())
    {
        return Err("Search contains unsupported control or formatting characters.".into());
    }
    let mut chars = text.chars().peekable();
    let mut terms = Vec::new();
    while chars.peek().is_some() {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let negative_prefix = chars.peek() == Some(&'-');
        let mut value = String::new();
        let mut quoted = false;
        while let Some(character) = chars.next() {
            if character == '"' {
                quoted = !quoted;
            } else if quoted
                && character == '\\'
                && chars.peek().is_some_and(|c| matches!(c, '"' | '\\'))
            {
                value.push(chars.next().expect("peeked character"));
            } else if character.is_whitespace() && !quoted {
                break;
            } else {
                value.push(character);
            }
        }
        if quoted {
            return Err("Close the double quote to finish this search phrase.".into());
        }
        let negative = negative_prefix && value.len() > 1;
        if negative {
            value.remove(0);
        }
        let (field, value) = match value.split_once(':') {
            Some((prefix, rest)) => match prefix.to_ascii_lowercase().as_str() {
                "title" | "name" => (SearchField::Title, rest),
                "id" => (SearchField::Id, rest),
                "provider" => (SearchField::Provider, rest),
                "project" | "cwd" => (SearchField::Project, rest),
                "branch" => (SearchField::Branch, rest),
                "tag" => (SearchField::Tag, rest),
                "status" => (SearchField::Status, rest),
                "is" => (SearchField::Is, rest),
                _ => (SearchField::Any, value.as_str()),
            },
            None => (SearchField::Any, value.as_str()),
        };
        if value.is_empty() {
            return Err("Add a value after the search field or inside the quotes.".into());
        }
        let path = (field == SearchField::Project)
            .then(|| normalized_absolute(Path::new(value)))
            .flatten();
        let value = value.to_lowercase();
        if field == SearchField::Is
            && !matches!(value.as_str(), "live" | "pinned" | "archived" | "resumed")
        {
            return Err("Use is:live, is:pinned, is:archived, or is:resumed.".into());
        }
        terms.push(SearchTerm {
            field,
            value,
            path,
            negative,
        });
        if terms.len() > MAX_QUERY_TERMS {
            return Err("Search is limited to 64 terms.".into());
        }
    }
    Ok(terms)
}

impl SearchEntry {
    fn new(session: &AgentSession, live: bool) -> Self {
        Self {
            provider: session.provider,
            status: session.status,
            id: session.id.to_lowercase(),
            key: session.id.clone(),
            library_key: library_key(session.provider, &session.id),
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
    fn score_terms(
        &self,
        query: &str,
        terms: &[SearchTerm],
        facts: &LibraryFacts<'_>,
    ) -> Option<u64> {
        // Empty browsing has no terms and no exact-match bonus to compute.
        if terms.is_empty() {
            return Some(0);
        }
        let alias = facts.alias.map(str::to_lowercase).unwrap_or_default();
        let tags = facts.tags;
        let mut total = 0;
        for term in terms {
            let value = term.value.as_str();
            let score = match term.field {
                SearchField::Any => self
                    .score("", &[value])
                    .unwrap_or(0)
                    .max(u64::from(field_score(&alias, value, 40_000)))
                    .max(
                        tags.iter()
                            .map(|tag| u64::from(field_score(tag, value, 10_000)))
                            .max()
                            .unwrap_or(0),
                    ),
                SearchField::Title => u64::from(
                    field_score(&self.title, value, 30_000).max(field_score(&alias, value, 40_000)),
                ),
                SearchField::Id => u64::from(field_score(&self.id, value, 100_000)),
                SearchField::Provider => u64::from(self.provider_name == value),
                SearchField::Project => match &term.path {
                    Some(root) => u64::from(
                        self.directory
                            .as_ref()
                            .is_some_and(|path| path.starts_with(root)),
                    ),
                    None => u64::from(field_score(&self.cwd, value, 5_000)),
                },
                SearchField::Branch => u64::from(field_score(&self.branch, value, 5_000)),
                SearchField::Tag => u64::from(tags.iter().any(|tag| tag == value)),
                SearchField::Status => u64::from(match value {
                    "working" | "running" => self.status == SessionStatus::Working,
                    "waiting" | "needs-input" | "approval" => self.status == SessionStatus::Waiting,
                    "idle" | "ready" => self.status == SessionStatus::Idle,
                    "unknown" => self.status == SessionStatus::Unknown,
                    "archived" => self.status == SessionStatus::Archived,
                    _ => false,
                }),
                SearchField::Is => u64::from(match value {
                    "live" => self.live,
                    "pinned" => facts.pinned,
                    "archived" => facts.archived,
                    "resumed" => facts.last_resumed_ms.is_some(),
                    _ => false,
                }),
            };
            if (score > 0) == term.negative {
                return None;
            }
            if !term.negative {
                total += score;
            }
        }
        if !query.is_empty() {
            if self.id == query {
                total += 1_000_000_000;
            }
            if self.title == query || alias == query {
                total += 100_000;
            }
        }
        Some(total)
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
                    pinned_first: false,
                    ..SessionLibraryFilter::default()
                }
            ),
            [1]
        );
    }

    #[test]
    fn management_query_combines_alias_tags_provider_project_and_exclusions() {
        use crate::agent_sessions::SessionOrganization;
        let id = "11111111-1111-4111-8111-111111111111";
        let catalog = catalog(vec![
            session(id, "Original provider title", "/work/repo", "", 1),
            session("other", "Renderer", "/work/repo-other", "", 2),
        ]);
        let library = SessionLibrarySnapshot::default()
            .with_organization(
                SessionProvider::Codex,
                id,
                SessionOrganization {
                    alias: Some("Renderer project".into()),
                    tags: vec!["GUI".into(), "needs review".into()],
                    archived: false,
                },
            )
            .unwrap();
        let index = SessionSearchIndex::new(&catalog);
        for text in [
            "renderer gui",
            "provider:codex project:/work/repo tag:gui",
            "title:\"renderer project\" -tag:done",
            "tag:\"needs review\"",
            "name:renderer -provider:claude tag:gui",
        ] {
            assert_eq!(
                index.query_with_library(
                    &SessionQuery {
                        text,
                        ..Default::default()
                    },
                    &library,
                    SessionLibraryFilter::default()
                ),
                [0],
                "{text}"
            );
        }
        for text in [
            "tag:gu",
            "tag:gui -title:renderer",
            "provider:claude",
            "project:/work/repo/subdir",
        ] {
            assert!(
                index
                    .query_with_library(
                        &SessionQuery {
                            text,
                            ..Default::default()
                        },
                        &library,
                        SessionLibraryFilter::default()
                    )
                    .is_empty(),
                "{text}"
            );
        }
        // Both the original provider title and local alias remain searchable.
        assert_eq!(
            index.query_with_library(
                &SessionQuery {
                    text: "original provider",
                    ..Default::default()
                },
                &library,
                SessionLibraryFilter::default()
            ),
            [0]
        );
    }

    #[test]
    fn archive_is_reversible_local_metadata_and_not_provider_status_or_liveness() {
        use crate::agent_sessions::SessionOrganization;
        let id = "11111111-1111-4111-8111-111111111111";
        let other = "22222222-2222-4222-8222-222222222222";
        let mut provider_archived = session(other, "Provider archive", "/repo", "", 2);
        provider_archived.archived = true;
        provider_archived.status = SessionStatus::Archived;
        let catalog = catalog(vec![
            session(id, "Local archive", "/repo", "", 1),
            provider_archived,
        ]);
        let before = catalog.clone();
        let index = SessionSearchIndex::new(&catalog);
        let library = SessionLibrarySnapshot::default()
            .with_organization(
                SessionProvider::Codex,
                id,
                SessionOrganization {
                    archived: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            index.query_with_library(
                &SessionQuery::default(),
                &library,
                SessionLibraryFilter::default()
            ),
            [1]
        );
        assert_eq!(
            index.query_with_library(
                &SessionQuery::default(),
                &library,
                SessionLibraryFilter {
                    archive: SessionArchiveFilter::Archived,
                    ..Default::default()
                }
            ),
            [0]
        );
        assert_eq!(
            index.query_with_library(
                &SessionQuery {
                    text: "is:archived",
                    ..Default::default()
                },
                &library,
                SessionLibraryFilter::default()
            ),
            [0]
        );
        assert_eq!(
            index.query_with_library(
                &SessionQuery {
                    text: "status:archived",
                    ..Default::default()
                },
                &library,
                SessionLibraryFilter::default()
            ),
            [1]
        );
        assert!(index
            .query_with_library(
                &SessionQuery {
                    text: "is:live",
                    ..Default::default()
                },
                &library,
                SessionLibraryFilter {
                    archive: SessionArchiveFilter::All,
                    ..Default::default()
                }
            )
            .is_empty());
        let restored = library
            .with_organization(SessionProvider::Codex, id, SessionOrganization::default())
            .unwrap();
        assert_eq!(
            index.query_with_library(
                &SessionQuery::default(),
                &restored,
                SessionLibraryFilter::default()
            ),
            [1, 0]
        );
        assert_eq!(catalog, before);
    }

    #[test]
    fn structured_search_reports_errors_and_preserves_literal_urls_and_quoted_minuses() {
        let catalog = catalog(vec![session(
            "one",
            "-renderer https://example.com \"hello\"",
            "/work/repo",
            "",
            1,
        )]);
        let index = SessionSearchIndex::new(&catalog);
        for text in [
            "tag:",
            "\"unfinished",
            "is:unknown",
            "\u{202e}hidden",
            "\"\"",
        ] {
            assert!(SessionSearchIndex::query_error(text).is_some(), "{text}");
            assert!(index
                .query(&SessionQuery {
                    text,
                    ..Default::default()
                })
                .is_empty());
        }
        for text in ["\"-renderer\"", "https://example.com", "\"\\\"hello\\\"\""] {
            assert!(SessionSearchIndex::query_error(text).is_none(), "{text}");
            assert_eq!(
                index.query(&SessionQuery {
                    text,
                    ..Default::default()
                }),
                [0],
                "{text}"
            );
        }
    }

    #[test]
    fn project_summaries_match_descendant_scope_and_separate_waiting_from_live() {
        use crate::agent_sessions::{SessionLibraryEntry, SessionOrganization};
        let a = "11111111-1111-4111-8111-111111111111";
        let b = "22222222-2222-4222-8222-222222222222";
        let mut waiting = session(a, "Recorded waiting", "/work/repo", "", 1);
        waiting.status = SessionStatus::Waiting;
        let mut catalog = catalog(vec![
            waiting,
            session(b, "Archived child", "/work/repo/subdir", "", 5),
            session("sibling", "Sibling", "/work/repo-other", "", 8),
        ]);
        catalog.live_processes.push(LiveSessionProcess {
            provider: SessionProvider::Codex,
            id: "sibling".into(),
            pid: 1234,
        });
        let library = SessionLibrarySnapshot::from_entries(vec![SessionLibraryEntry {
            provider: SessionProvider::Codex,
            id: a.into(),
            pinned: true,
            last_resumed_ms: None,
        }])
        .unwrap()
        .with_organization(
            SessionProvider::Codex,
            b,
            SessionOrganization {
                archived: true,
                ..Default::default()
            },
        )
        .unwrap();
        let index = SessionSearchIndex::new(&catalog);
        let projects = index.projects(&library);
        let project = projects
            .iter()
            .find(|project| project.root == Path::new("/work/repo"))
            .unwrap();
        assert_eq!(
            (
                project.total,
                project.waiting,
                project.live,
                project.pinned,
                project.archived,
                project.updated_at_ms
            ),
            (1, 1, 0, 1, 1, 5)
        );
        assert_eq!(
            index
                .query_with_library(
                    &SessionQuery {
                        project: Some(&project.root),
                        ..Default::default()
                    },
                    &library,
                    SessionLibraryFilter::default()
                )
                .len(),
            project.total
        );
        assert_eq!(
            projects
                .iter()
                .find(|project| project.root == Path::new("/work/repo-other"))
                .unwrap()
                .live,
            1
        );
    }

    #[test]
    fn exact_session_identity_still_outranks_a_local_alias_and_tags() {
        use crate::agent_sessions::SessionOrganization;
        let a = "11111111-1111-4111-8111-111111111111";
        let b = "22222222-2222-4222-8222-222222222222";
        let index = SessionSearchIndex::new(&catalog(vec![
            session(a, "Exact UUID", "/repo", "", 1),
            session(b, "Newer alias", "/repo", "", 20),
        ]));
        let library = SessionLibrarySnapshot::default()
            .with_organization(
                SessionProvider::Codex,
                b,
                SessionOrganization {
                    alias: Some(a.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            index.query_with_library(
                &SessionQuery {
                    text: a,
                    ..Default::default()
                },
                &library,
                SessionLibraryFilter::default()
            ),
            [0, 1]
        );
    }
}
