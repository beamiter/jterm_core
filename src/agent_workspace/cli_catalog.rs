//! Read-only catalog preparation for provider CLI sessions. The workspace
//! daemon owns persistence, runtime leases and source revalidation.
use super::{ConversationId, WorkspaceRow};
use crate::agent_sessions::{
    AgentSession, SessionCatalog, SessionLibrarySnapshot, SessionProvider,
};
use std::collections::HashMap;
use std::path::PathBuf;

type SourceKey = (SessionProvider, String, PathBuf);
const MAX_INDEX_ROWS: usize = 2_048;

/// Maps a specific provider/native-ID/source association to the canonical
/// identity resolved at worker admission time. Not launch/deletion authority.
#[derive(Default)]
pub struct WorkspaceCliCatalogIndex {
    identities: HashMap<SourceKey, ConversationId>,
}

impl WorkspaceCliCatalogIndex {
    /// Prepares an identity lookup and bounded IndexSessions rows, without
    /// sending commands or changing persistent state. Run on a worker: source
    /// resolution may inspect the filesystem. Repeated exact provider/native IDs do not
    /// transfer local organization to any source.
    pub fn prepare(
        catalog: &SessionCatalog,
        legacy: &SessionLibrarySnapshot,
    ) -> (Self, Vec<WorkspaceRow>) {
        let mut counts = HashMap::new();
        for session in &catalog.sessions {
            *counts
                .entry((session.provider, session.id.as_str()))
                .or_insert(0) += 1;
        }
        let mut result = Self::default();
        let mut rows = Vec::new();
        for session in &catalog.sessions {
            let Ok(mut row) = WorkspaceRow::from_agent(session) else {
                continue;
            };
            result.identities.insert(raw_key(session), row.id.clone());
            if counts[&(session.provider, session.id.as_str())] == 1 {
                let facts = legacy.facts(session.provider, &session.id);
                row.title = facts.display_title(&session.title).into();
                row.pinned = facts.pinned;
                row.archived |= facts.archived;
                row.tags = facts.tags.to_vec();
            }
            rows.push(row);
        }
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        rows.truncate(MAX_INDEX_ROWS);
        (result, rows)
    }

    /// Reuse a worker-established lookup, not a provider-only legacy key.
    pub fn identity(&self, session: &AgentSession) -> Option<&ConversationId> {
        self.identities.get(&raw_key(session))
    }
}

fn raw_key(session: &AgentSession) -> SourceKey {
    (session.provider, session.id.clone(), session.source.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent_sessions::{SessionLibraryEntry, SessionOrganization, SessionStatus},
        agent_workspace::{ConversationKind, RuntimeState},
    };
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let fixture = Self(
                std::env::temp_dir().join(format!("cli-index-{}-{nonce}", std::process::id())),
            );
            fs::create_dir(&fixture.0).unwrap();
            for path in ["one/sessions", "two/sessions", "one/projects"] {
                fs::create_dir_all(fixture.0.join(path)).unwrap();
            }
            fixture
        }

        fn session(&self, relative_source: &str, native_id: &str) -> AgentSession {
            AgentSession {
                provider: SessionProvider::Codex,
                id: native_id.into(),
                title: "Provider title".into(),
                cwd: self.0.clone(),
                updated_at_ms: 123,
                status: SessionStatus::Waiting,
                preview: "Provider preview".into(),
                branch: Some("feature/example".into()),
                source: self.0.join(relative_source),
                archived: false,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn catalog(sessions: Vec<AgentSession>) -> SessionCatalog {
        SessionCatalog {
            sessions,
            ..Default::default()
        }
    }

    #[test]
    fn real_cli_rows_preserve_source_identity_and_only_unambiguous_organization() {
        let f = Fixture::new();
        let native_id = "00000000-0000-4000-8000-000000000101";
        let first = f.session("one/sessions/a.jsonl", native_id);
        let mut second = f.session("two/sessions/b.jsonl", native_id);
        second.archived = true;
        let mut claude = f.session("one/projects/c.jsonl", native_id);
        claude.provider = SessionProvider::Claude;
        let unavailable = f.session("missing/sessions/d.jsonl", native_id);
        let mut different_path = first.clone();
        different_path.source.set_file_name("not-indexed.jsonl");
        let preview_bytes = b"catalog preparation must not rewrite this source
";
        fs::write(&first.source, preview_bytes).unwrap();

        let legacy = SessionLibrarySnapshot::from_entries(vec![SessionLibraryEntry {
            provider: SessionProvider::Codex,
            id: native_id.into(),
            pinned: true,
            last_resumed_ms: Some(456),
        }])
        .unwrap()
        .with_organization(
            SessionProvider::Codex,
            native_id,
            SessionOrganization {
                alias: Some(" Local alias ".into()),
                tags: vec![" REVIEW ".into()],
                archived: true,
            },
        )
        .unwrap();

        let first_base = WorkspaceRow::from_agent(&first).unwrap();
        let second_base = WorkspaceRow::from_agent(&second).unwrap();
        let claude_base = WorkspaceRow::from_agent(&claude).unwrap();
        assert_eq!(first_base.kind, ConversationKind::Cli);
        assert_eq!(first_base.runtime, RuntimeState::Idle);
        assert_eq!(
            first_base.facts.recorded_status,
            Some(SessionStatus::Waiting)
        );
        assert_ne!(first_base.id, second_base.id);
        assert_ne!(first_base.id, claude_base.id);
        assert!(second_base.archived);

        let (index, rows) = WorkspaceCliCatalogIndex::prepare(
            &catalog(vec![claude.clone(), first.clone()]),
            &legacy,
        );
        let mut decorated = first_base.clone();
        decorated.title = "Local alias".into();
        decorated.pinned = true;
        decorated.archived = true;
        decorated.tags = vec!["review".into()];
        let mut expected = vec![decorated, claude_base.clone()];
        expected.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(rows, expected);
        assert_eq!(index.identity(&different_path), None);

        let (index, rows) = WorkspaceCliCatalogIndex::prepare(
            &catalog(vec![second.clone(), claude.clone(), first.clone()]),
            &legacy,
        );
        let mut expected = vec![first_base.clone(), second_base.clone(), claude_base.clone()];
        expected.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(rows, expected);
        assert_eq!(index.identity(&first), Some(&first_base.id));
        assert_eq!(index.identity(&second), Some(&second_base.id));
        assert_eq!(index.identity(&claude), Some(&claude_base.id));

        // Still ambiguous when one source does not resolve. Its missing path
        // must not cause the surviving source to inherit legacy organization.
        let (index, rows) = WorkspaceCliCatalogIndex::prepare(
            &catalog(vec![first.clone(), unavailable.clone()]),
            &legacy,
        );
        assert_eq!(index.identity(&first), Some(&first_base.id));
        assert_eq!(rows, [first_base]);
        assert_eq!(index.identity(&unavailable), None);
        assert_eq!(fs::read(&first.source).unwrap(), preview_bytes);
    }

    #[test]
    fn cap_and_order_write_batch_without_altering_the_source_lookup() {
        let f = Fixture::new();
        let make_session = |n| {
            f.session(
                "one/sessions/fixture.jsonl",
                &format!("00000000-0000-4000-8000-{n:012x}"),
            )
        };
        let catalog = catalog((0..MAX_INDEX_ROWS + 2).rev().map(make_session).collect());
        let (index, rows) =
            WorkspaceCliCatalogIndex::prepare(&catalog, &SessionLibrarySnapshot::default());
        assert_eq!(rows.len(), MAX_INDEX_ROWS);
        for (n, row) in rows.iter().enumerate() {
            assert_eq!(
                row.id,
                WorkspaceRow::from_agent(&make_session(n)).unwrap().id
            );
        }
        assert!(rows.windows(2).all(|rows| rows[0].id < rows[1].id));
        for n in MAX_INDEX_ROWS..MAX_INDEX_ROWS + 2 {
            let session = make_session(n);
            let id = WorkspaceRow::from_agent(&session).unwrap().id;
            assert_eq!(index.identity(&session), Some(&id));
            assert!(!rows.iter().any(|row| row.id == id));
        }
    }
}
