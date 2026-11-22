//! Source-qualified shared organization for provider CLI conversations.
//! The provider history remains the resume authority; SQLite owns display metadata.
use crate::agent_sessions::{
    AgentSession, SessionBatchAction, SessionCatalog, SessionLibraryEntry, SessionLibrarySnapshot,
    SessionOrganization, SessionProvider,
};
use crate::agent_workspace::{
    session_id, ConversationId, ConversationKind, WorkspaceCliCatalogIndex, WorkspaceClient,
    WorkspaceCommand as Command, WorkspaceEvent, WorkspaceOrganization, WorkspaceReply,
    WorkspaceRow,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// Pure source key for widgets and live terminal bindings. Canonical identity
/// and filesystem validation happen in the index worker / managed launch.
pub fn source_profile(session: &AgentSession) -> PathBuf {
    let boundaries: &[&str] = match session.provider {
        SessionProvider::Codex => &["sessions", "archived_sessions"],
        SessionProvider::Claude => &["projects"],
    };
    session
        .source
        .ancestors()
        .skip(1)
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| boundaries.contains(&name))
        })
        .and_then(|path| path.parent())
        .map(PathBuf::from)
        .unwrap_or_else(|| session.source.clone())
}
enum Pending {
    Refresh,
    Write,
    Delete(ConversationId),
}

pub struct WorkspaceSessions {
    client: WorkspaceClient,
    pending: HashMap<u64, Pending>,
    deleted: HashSet<ConversationId>,
    deletion_updates: Vec<Result<ConversationId, String>>,
    indexed: Vec<WorkspaceRow>,
    identities: WorkspaceCliCatalogIndex,
    index_job: Option<Receiver<(WorkspaceCliCatalogIndex, Vec<WorkspaceRow>)>>,
    last_catalog: Option<SessionCatalog>,
    last_legacy: Option<SessionLibrarySnapshot>,
    pub rows: HashMap<ConversationId, WorkspaceRow>,
    pub notice: Option<String>,
    refresh_at: Instant,
}

impl Default for WorkspaceSessions {
    fn default() -> Self {
        Self::with_client(WorkspaceClient::new())
    }
}

impl WorkspaceSessions {
    /// Use an explicit Workspace endpoint when the frontend owns the client.
    pub fn with_client(client: WorkspaceClient) -> Self {
        Self {
            client,
            pending: HashMap::new(),
            deleted: HashSet::new(),
            deletion_updates: Vec::new(),
            indexed: Vec::new(),
            identities: WorkspaceCliCatalogIndex::default(),
            index_job: None,
            last_catalog: None,
            last_legacy: None,
            rows: HashMap::new(),
            notice: None,
            refresh_at: Instant::now() - Duration::from_secs(10),
        }
    }
    fn send(&mut self, command: Command, pending: Pending) -> Result<(), String> {
        let request = self.client.send(command)?;
        self.pending.insert(request, pending);
        Ok(())
    }

    pub fn delete(&mut self, id: ConversationId, session: AgentSession) -> Result<(), String> {
        if self.row(&session).is_none_or(|row| row.id != id) {
            return Err("This session source changed. Refresh before deleting it.".into());
        }
        if self
            .pending
            .values()
            .any(|pending| matches!(pending, Pending::Delete(target) if target == &id))
        {
            return Err("This session is already being deleted.".into());
        }
        self.send(
            Command::DeleteCliSession {
                id: id.clone(),
                session: Box::new(session),
            },
            Pending::Delete(id),
        )
    }

    pub fn is_deleted(&self, session: &AgentSession) -> bool {
        self.identities
            .identity(session)
            .is_some_and(|id| self.deleted.contains(id))
    }

    pub fn take_deletion_updates(&mut self) -> Vec<Result<ConversationId, String>> {
        std::mem::take(&mut self.deletion_updates)
    }
    pub fn refresh(&mut self) {
        if !self
            .pending
            .values()
            .any(|pending| matches!(pending, Pending::Refresh))
        {
            self.refresh_at = Instant::now();
            if let Err(error) = self.send(Command::Refresh, Pending::Refresh) {
                self.notice = Some(error);
            }
        }
    }
    /// First import may adopt legacy metadata only when the native identity has
    /// exactly one source. Later indexing cannot overwrite shared user edits.
    pub fn sync(&mut self, catalog: &SessionCatalog, legacy: &SessionLibrarySnapshot) -> bool {
        let mut changed = false;
        for WorkspaceEvent::Reply { request_id, result } in self.client.poll() {
            let Some(pending) = self.pending.remove(&request_id) else {
                continue;
            };
            match (pending, result) {
                (Pending::Refresh, Ok(WorkspaceReply::Catalog { rows })) => {
                    let rows = rows
                        .into_iter()
                        .filter(|row| row.kind == ConversationKind::Cli)
                        .map(|row| (row.id.clone(), row))
                        .collect();
                    changed |= self.rows != rows;
                    self.rows = rows;
                    self.notice = None;
                }
                (Pending::Delete(id), Ok(WorkspaceReply::Ack)) => {
                    self.rows.remove(&id);
                    self.deleted.insert(id.clone());
                    self.deletion_updates.push(Ok(id));
                    changed = true;
                    self.refresh();
                }
                (Pending::Delete(_), Err(error)) => self.deletion_updates.push(Err(error)),
                (Pending::Delete(_), Ok(_)) => self.deletion_updates.push(Err(
                    "Unexpected acknowledgement for session deletion.".into(),
                )),
                (Pending::Write, Ok(_)) => self.refresh(),
                (_, Err(error)) => self.notice = Some(error),
                _ => {}
            }
        }
        if let Some((identities, rows)) =
            self.index_job.as_ref().and_then(|job| job.try_recv().ok())
        {
            self.index_job = None;
            self.identities = identities;
            if rows != self.indexed {
                match self.send(
                    Command::IndexSessions {
                        sessions: rows.clone(),
                    },
                    Pending::Write,
                ) {
                    Ok(()) => self.indexed = rows,
                    Err(error) => {
                        self.notice = Some(error);
                        self.last_catalog = None;
                    }
                }
            }
            changed = true;
        }
        if self.index_job.is_none()
            && (self.last_catalog.as_ref() != Some(catalog)
                || self.last_legacy.as_ref() != Some(legacy))
        {
            let snapshot = catalog.clone();
            let metadata = legacy.clone();
            self.last_catalog = Some(snapshot.clone());
            self.last_legacy = Some(metadata.clone());
            let (send, receive) = mpsc::sync_channel(1);
            match std::thread::Builder::new()
                .name("workspace-session-index".into())
                .spawn(move || {
                    let _ = send.send(WorkspaceCliCatalogIndex::prepare(&snapshot, &metadata));
                }) {
                Ok(_) => self.index_job = Some(receive),
                Err(error) => {
                    self.notice = Some(error.to_string());
                    self.last_catalog = None;
                }
            }
        }
        if self.refresh_at.elapsed() >= Duration::from_secs(5) {
            self.refresh();
        }
        changed
    }
    pub fn busy(&self) -> bool {
        self.pending
            .values()
            .any(|pending| matches!(pending, Pending::Write))
    }
    pub fn projection(
        &self,
        catalog: &SessionCatalog,
        legacy: &SessionLibrarySnapshot,
    ) -> SessionLibrarySnapshot {
        if self.rows.is_empty() {
            return legacy.clone();
        }
        let mut counts = HashMap::new();
        for session in &catalog.sessions {
            *counts
                .entry((session.provider, session.id.as_str()))
                .or_insert(0) += 1;
        }
        let entries = catalog
            .sessions
            .iter()
            .filter(|session| {
                counts[&(session.provider, session.id.as_str())] == 1
                    && uuid::Uuid::parse_str(&session.id).is_ok()
            })
            .map(|session| {
                let facts = legacy.facts(session.provider, &session.id);
                SessionLibraryEntry {
                    provider: session.provider,
                    id: session.id.clone(),
                    pinned: self.row(session).map_or(facts.pinned, |row| row.pinned),
                    last_resumed_ms: facts.last_resumed_ms,
                }
            })
            .collect();
        let mut snapshot = SessionLibrarySnapshot::from_entries(entries).unwrap_or_default();
        for session in &catalog.sessions {
            if counts[&(session.provider, session.id.as_str())] != 1
                || uuid::Uuid::parse_str(&session.id).is_err()
            {
                continue;
            }
            let facts = legacy.facts(session.provider, &session.id);
            let organization = if let Some(row) = self.row(session) {
                SessionOrganization {
                    alias: (row.title != session.title && row.title.len() <= 160)
                        .then(|| row.title.clone()),
                    archived: row.archived,
                    tags: row.tags.clone(),
                }
            } else {
                SessionOrganization {
                    alias: facts.alias.map(str::to_owned),
                    archived: facts.archived,
                    tags: facts.tags.to_vec(),
                }
            };
            if let Ok(updated) =
                snapshot
                    .clone()
                    .with_organization(session.provider, &session.id, organization)
            {
                snapshot = updated;
            }
        }
        snapshot
    }
    pub fn apply_batch(
        &mut self,
        sessions: &[AgentSession],
        action: &SessionBatchAction,
    ) -> Result<usize, String> {
        let mut changes = Vec::new();
        for session in sessions {
            let row = self
                .row(session)
                .ok_or("Shared metadata is still loading. Try again shortly.")?;
            let mut change = WorkspaceOrganization::new(row.id.clone());
            match action {
                SessionBatchAction::Pin(pinned) => change.pinned = Some(*pinned),
                SessionBatchAction::Archive(archived) => change.archived = Some(*archived),
                SessionBatchAction::AddTag(tag) | SessionBatchAction::RemoveTag(tag) => {
                    crate::agent_sessions::validate_session_batch_tag(tag)?;
                    let mut tags = row.tags.clone();
                    if matches!(action, SessionBatchAction::AddTag(_)) {
                        tags.push(tag.trim().to_owned());
                    } else {
                        tags.retain(|old| old != tag.trim());
                    }
                    tags.sort();
                    tags.dedup();
                    change.tags = Some(tags);
                }
            }
            changes.push(change);
        }
        self.send(Command::Organize { changes }, Pending::Write)?;
        Ok(sessions.len())
    }
    pub fn row(&self, session: &AgentSession) -> Option<&WorkspaceRow> {
        self.rows.get(self.identities.identity(session)?)
    }
    pub fn pinned(&self, session: &AgentSession) -> bool {
        self.row(session).is_some_and(|row| row.pinned)
    }
    pub fn title<'a>(&'a self, session: &'a AgentSession) -> &'a str {
        self.row(session)
            .map(|row| row.title.as_str())
            .unwrap_or(&session.title)
    }
    pub fn set_pinned(&mut self, session: &AgentSession, pinned: bool) -> Result<(), String> {
        self.send(
            Command::Pin {
                id: session_id(session)?,
                pinned,
            },
            Pending::Write,
        )
    }
    pub fn set_archived(&mut self, session: &AgentSession, archived: bool) -> Result<(), String> {
        self.send(
            Command::Archive {
                id: session_id(session)?,
                archived,
            },
            Pending::Write,
        )
    }
    pub fn set_details(
        &mut self,
        session: &AgentSession,
        title: &str,
        tags: &str,
    ) -> Result<(), String> {
        crate::agent_sessions::validate_session_details(title, tags)?;
        let title = (!title.trim().is_empty()).then(|| title.trim().to_owned());
        let mut tags: Vec<_> = tags
            .split(',')
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .map(str::to_owned)
            .collect();
        tags.sort();
        tags.dedup();
        let id = session_id(session)?;
        let mut change = WorkspaceOrganization::new(id);
        change.title = Some(title.unwrap_or_else(|| session.title.clone()));
        change.tags = Some(tags);
        self.send(
            Command::Organize {
                changes: vec![change],
            },
            Pending::Write,
        )
    }
}
