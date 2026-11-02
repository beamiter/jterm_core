//! The host thread is the sole writer. ChatStore is a bounded active window;
//! this database keeps complete successful turns and pages older messages.
use super::model::*;
use super::ownership::{private_file, process_matches, Lease, Paths};
use super::protocol::{WorkspaceCommand, WorkspaceReply};
use crate::ai::{ChatSnapshot, ConversationSnapshot, Role, Turn};
use rusqlite::{params, Connection, OptionalExtension};
use std::time::Duration;
mod hosted_api;

const MAX_MESSAGE: usize = super::api::API_RESULT_BYTES;
const MAX_DRAFT: usize = 64 * 1024;
const MAX_ROWS: usize = 4096;

pub(super) struct Store {
    db: Connection,
    _writer: Lease,
    paths: Paths,
}

impl Store {
    pub fn open(paths: &Paths) -> Result<Self, String> {
        let writer = Lease::acquire(&paths.state.join("writer.lock"))?;
        let path = paths.state.join("workspace.sqlite3");
        drop(private_file(&path)?);
        let db = Connection::open(&path).map_err(err)?;
        db.busy_timeout(Duration::from_secs(2)).map_err(err)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
            INSERT INTO schema_version(version) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM schema_version);
            CREATE TABLE IF NOT EXISTS conversations (
                numeric_id INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT NOT NULL UNIQUE, row_json TEXT NOT NULL,
                revision INTEGER NOT NULL DEFAULT 0, draft_revision INTEGER NOT NULL DEFAULT 0,
                epoch INTEGER NOT NULL DEFAULT 0, draft TEXT NOT NULL DEFAULT '',
                template TEXT NOT NULL DEFAULT '', history_truncated INTEGER NOT NULL DEFAULT 0,
                active_nonce TEXT, submitted_text TEXT, submitted_draft_revision INTEGER,
                owner_pid INTEGER, owner_start TEXT);
            CREATE TABLE IF NOT EXISTS messages (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                role TEXT NOT NULL CHECK(role IN ('user','assistant')), body TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS messages_conversation ON messages(conversation_id,sequence);
            CREATE TABLE IF NOT EXISTS legacy_imports (
                namespace TEXT NOT NULL, legacy_id INTEGER NOT NULL,
                conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                PRIMARY KEY(namespace,legacy_id));
            CREATE TABLE IF NOT EXISTS read_positions (
                conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                view TEXT NOT NULL, position REAL NOT NULL, updated_at_ms INTEGER NOT NULL,
                PRIMARY KEY(conversation_id,view));").map_err(err)?;
        let version: i64 = db
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
                r.get(0)
            })
            .map_err(err)?;
        if !(1..=3).contains(&version) {
            return Err(format!(
                "Unsupported Agent Workspace database version {version}"
            ));
        }
        if version == 1 {
            let has_owner = db
                .prepare("SELECT owner_pid FROM conversations LIMIT 0")
                .is_ok();
            if !has_owner {
                db.execute_batch("BEGIN; ALTER TABLE conversations ADD COLUMN owner_pid INTEGER; ALTER TABLE conversations ADD COLUMN owner_start TEXT; UPDATE schema_version SET version=2; COMMIT;").map_err(err)?;
            } else {
                db.execute("UPDATE schema_version SET version=2", [])
                    .map_err(err)?;
            }
        }
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS api_jobs (
            id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
            epoch INTEGER NOT NULL, nonce TEXT NOT NULL, draft_revision INTEGER NOT NULL,
            snapshot TEXT NOT NULL);
            UPDATE schema_version SET version=3;",
        )
        .map_err(err)?;
        // A restarted host cannot continue an API request owned by its old
        // process. Revoke its epoch, preserving a newer draft exactly.
        // API requests execute in frontend workers, which may still be alive
        // when a host restarts. A restart alone does not revoke their lease.
        let store = Self {
            db,
            _writer: writer,
            paths: paths.clone(),
        };
        store.recover_api_jobs()?;
        store.reap_abandoned_requests()?;
        Ok(store)
    }

    pub fn rows(&self) -> Result<Vec<WorkspaceRow>, String> {
        let mut stmt = self
            .db
            .prepare(
                "SELECT row_json,active_nonce FROM conversations ORDER BY numeric_id DESC LIMIT ?1",
            )
            .map_err(err)?;
        let mapped = stmt
            .query_map([MAX_ROWS as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .map_err(err)?;
        let mut rows = Vec::new();
        for value in mapped {
            let (json, active) = value.map_err(err)?;
            let mut row: WorkspaceRow = serde_json::from_str(&json).map_err(err)?;
            if row.kind == ConversationKind::Api {
                row.runtime = if let Some(nonce) = active {
                    let hosted: Option<String> = self
                        .db
                        .query_row(
                            "SELECT snapshot FROM api_jobs WHERE id=?1 AND nonce=?2",
                            params![row.id.0, nonce],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(err)?;
                    if hosted
                        .as_deref()
                        .map(serde_json::from_str::<super::api::ApiSnapshot>)
                        .transpose()
                        .map_err(err)?
                        .is_some_and(|job| !job.state.is_live())
                    {
                        RuntimeState::Conflict
                    } else {
                        RuntimeState::Running
                    }
                } else {
                    RuntimeState::Idle
                };
            }
            rows.push(row);
        }
        rows.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.updated_at_ms.cmp(&a.updated_at_ms))
                .then(a.id.cmp(&b.id))
        });
        Ok(rows)
    }

    pub fn upsert_runtime_row(&self, row: &WorkspaceRow) -> Result<(), String> {
        let mut row = row.clone();
        if let Some(existing) = self.row(&row.id)? {
            row.title = existing.title;
            row.archived = existing.archived;
            row.pinned = existing.pinned;
            row.tags = existing.tags;
        }
        let json = serde_json::to_string(&row).map_err(err)?;
        self.db.execute("INSERT INTO conversations(id,row_json) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET row_json=excluded.row_json",params![row.id.0,json]).map_err(err)?;
        Ok(())
    }

    fn row(&self, id: &ConversationId) -> Result<Option<WorkspaceRow>, String> {
        self.db
            .query_row(
                "SELECT row_json FROM conversations WHERE id=?1",
                [&id.0],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(err)?
            .map(|s| serde_json::from_str(&s).map_err(err))
            .transpose()
    }

    #[cfg(test)]
    pub fn command(&mut self, command: WorkspaceCommand) -> Result<WorkspaceReply, String> {
        let pid = std::process::id();
        let start = super::ownership::process_start(pid)
            .ok_or("Cannot identify workspace client process")?;
        self.command_as(command, (pid, start))
    }

    pub fn command_as(
        &mut self,
        command: WorkspaceCommand,
        owner: (u32, String),
    ) -> Result<WorkspaceReply, String> {
        self.reap_abandoned_requests()?;
        match command {
            WorkspaceCommand::Refresh => Ok(WorkspaceReply::Catalog { rows: self.rows()? }),
            WorkspaceCommand::IndexSessions { sessions } => {
                if sessions.len() > 2048 {
                    return Err("Provider index exceeds its 2048-session budget".into());
                }
                for row in &sessions {
                    validate_index_row(row)?;
                }
                // One transaction makes a scanner refresh visible as one
                // projection, never a half-imported library after a crash.
                self.db.execute_batch("BEGIN IMMEDIATE").map_err(err)?;
                let result = (|| {
                    for mut row in sessions {
                        // Discovery is a metadata observation, not proof of
                        // a live process. Local task owners overlay their
                        // own runtime, and the host overlays managed PTYs.
                        row.runtime = RuntimeState::Idle;
                        self.upsert_runtime_row(&row)?;
                    }
                    Ok(WorkspaceReply::Ack)
                })();
                if result.is_ok() {
                    self.db.execute_batch("COMMIT").map_err(err)?;
                } else {
                    let _ = self.db.execute_batch("ROLLBACK");
                }
                result
            }
            WorkspaceCommand::ImportChats {
                namespace,
                snapshot_json,
            } => self.import(&namespace, &snapshot_json),
            WorkspaceCommand::LoadChat { id } => Ok(WorkspaceReply::Chat {
                chat: self.load(&id)?,
            }),
            WorkspaceCommand::CreateChat { title } => {
                check_text(&title, 256, "title")?;
                let id = ConversationId(format!("jterm:api:{}", uuid::Uuid::new_v4()));
                let row = api_row(id.clone(), &title);
                self.upsert_runtime_row(&row)?;
                Ok(WorkspaceReply::Chat {
                    chat: self.load(&id)?,
                })
            }
            WorkspaceCommand::SaveChat {
                id,
                expected_revision,
                snapshot_json,
            } => self.save(&id, expected_revision, &snapshot_json),
            WorkspaceCommand::SetDraft {
                id,
                expected_revision,
                text,
            } => {
                check_text(&text, MAX_DRAFT, "draft")?;
                let changed=self.db.execute("UPDATE conversations SET draft=?1,draft_revision=draft_revision+1,revision=revision+1 WHERE id=?2 AND draft_revision=?3",params![text,id.0,to_i64(expected_revision)?]).map_err(err)?;
                if changed != 1 {
                    return Err("Draft revision conflict: reload before saving; your local draft is unchanged".into());
                }
                Ok(WorkspaceReply::Chat {
                    chat: self.load(&id)?,
                })
            }
            WorkspaceCommand::Rename { id, title } => {
                check_text(&title, 256, "title")?;
                if title.trim().is_empty() {
                    return Err("Title cannot be empty".into());
                }
                self.mutate_row(&id, |r| r.title = title)?;
                Ok(WorkspaceReply::Ack)
            }
            WorkspaceCommand::Archive { id, archived } => {
                self.ensure_idle(&id)?;
                self.mutate_row(&id, |r| r.archived = archived)?;
                Ok(WorkspaceReply::Ack)
            }
            WorkspaceCommand::Pin { id, pinned } => {
                self.mutate_row(&id, |r| r.pinned = pinned)?;
                Ok(WorkspaceReply::Ack)
            }
            WorkspaceCommand::Tags { id, tags } => {
                validate_tags(&tags)?;
                let mut normalized = Vec::new();
                for tag in tags {
                    let tag = tag.trim().to_owned();
                    if !normalized
                        .iter()
                        .any(|v: &String| v.eq_ignore_ascii_case(&tag))
                    {
                        normalized.push(tag);
                    }
                }
                self.mutate_row(&id, |r| r.tags = normalized)?;
                Ok(WorkspaceReply::Ack)
            }
            WorkspaceCommand::Organize { changes } => {
                if changes.is_empty() || changes.len() > 128 {
                    return Err(
                        "Organization batches require between 1 and 128 conversations".into(),
                    );
                }
                let mut ids = std::collections::HashSet::new();
                for change in &changes {
                    if !ids.insert(change.id.clone()) {
                        return Err("Organization batch contains a duplicate identity".into());
                    }
                    if self.row(&change.id)?.is_none() {
                        return Err(
                            "An organized conversation no longer exists; refresh and retry".into(),
                        );
                    }
                    if let Some(title) = &change.title {
                        check_text(title, 256, "title")?;
                        if title.trim().is_empty() {
                            return Err("Title cannot be empty".into());
                        }
                    }
                    if let Some(tags) = &change.tags {
                        validate_tags(tags)?;
                    }
                    if change.archived.is_some() {
                        self.ensure_idle(&change.id)?;
                    }
                }
                self.db.execute_batch("BEGIN IMMEDIATE").map_err(err)?;
                let result = (|| {
                    for change in changes {
                        self.mutate_row(&change.id, |row| {
                            if let Some(title) = change.title {
                                row.title = title;
                            }
                            if let Some(pinned) = change.pinned {
                                row.pinned = pinned;
                            }
                            if let Some(archived) = change.archived {
                                row.archived = archived;
                            }
                            if let Some(tags) = change.tags {
                                row.tags = tags;
                            }
                        })?;
                    }
                    Ok(WorkspaceReply::Ack)
                })();
                if result.is_ok() {
                    self.db.execute_batch("COMMIT").map_err(err)?;
                } else {
                    let _ = self.db.execute_batch("ROLLBACK");
                }
                result
            }
            WorkspaceCommand::Delete { id } => {
                self.ensure_idle(&id)?;
                self.db
                    .execute("DELETE FROM conversations WHERE id=?1", [&id.0])
                    .map_err(err)?;
                Ok(WorkspaceReply::Ack)
            }
            WorkspaceCommand::History { id, before, limit } => {
                let messages = self.history(&id, before, limit.clamp(2, 100) / 2 * 2)?;
                let first = messages.first().map(|m| m.sequence);
                let next_before = if let Some(first) = first {
                    let older:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=?1 AND sequence<?2)",params![id.0,to_i64(first)?],|r|r.get(0)).map_err(err)?;
                    older.then_some(first)
                } else {
                    None
                };
                Ok(WorkspaceReply::Messages {
                    messages,
                    next_before,
                })
            }
            WorkspaceCommand::SetReadPosition { id, view, position } => {
                check_text(&view, 64, "reader view")?;
                if view.is_empty()
                    || !position.is_finite()
                    || !(0.0..=1_000_000_000.0).contains(&position)
                {
                    return Err("Invalid conversation reading position".into());
                }
                self.db.execute("INSERT INTO read_positions(conversation_id,view,position,updated_at_ms) VALUES(?1,?2,?3,?4) ON CONFLICT(conversation_id,view) DO UPDATE SET position=excluded.position,updated_at_ms=excluded.updated_at_ms",params![id.0,view,position,now_ms() as i64]).map_err(err)?;
                Ok(WorkspaceReply::Ack)
            }
            WorkspaceCommand::BeginSubmission {
                id,
                expected_revision,
                draft_revision,
                text,
            } => self.begin(&id, expected_revision, draft_revision, &text, owner),
            WorkspaceCommand::CompleteSubmission { receipt, text } => {
                self.complete(&receipt, &text)
            }
            WorkspaceCommand::CancelSubmission {
                receipt,
                restore_draft,
            } => self.cancel(&receipt, restore_draft),
            WorkspaceCommand::Stop { .. }
            | WorkspaceCommand::Runtime { .. }
            | WorkspaceCommand::StartApi { .. }
            | WorkspaceCommand::InspectApi { .. }
            | WorkspaceCommand::ResolveApi { .. }
            | WorkspaceCommand::AttachApi { .. }
            | WorkspaceCommand::DetachApi { .. }
            | WorkspaceCommand::CancelApi { .. } => {
                Err("Runtime operation must be handled by the session host".into())
            }
        }
    }

    /// A dead frontend cannot leave an API conversation permanently busy.
    /// PID reuse is distinguished using the kernel process start time; live
    /// owners are never replaced merely because a heartbeat is old.
    pub fn reap_abandoned_requests(&self) -> Result<(), String> {
        let mut stmt=self.db.prepare("SELECT id,active_nonce,owner_pid,owner_start FROM conversations WHERE active_nonce IS NOT NULL").map_err(err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<u32>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(err)?;
        let mut stale = Vec::new();
        for row in rows {
            let (id, nonce, pid, start) = row.map_err(err)?;
            if !pid
                .zip(start)
                .is_some_and(|(pid, start)| process_matches(pid, &start))
                && !super::ownership::lease_is_held(&self.paths.lease(&ConversationId(id.clone())))
            {
                stale.push((id, nonce));
            }
        }
        drop(stmt);
        for (id, nonce) in stale {
            self.db.execute("UPDATE conversations SET draft=CASE WHEN draft_revision=submitted_draft_revision AND (draft='' OR draft=submitted_text) THEN COALESCE(submitted_text,draft) ELSE draft END,draft_revision=draft_revision+CASE WHEN draft_revision=submitted_draft_revision AND (draft='' OR draft=submitted_text) THEN 1 ELSE 0 END,revision=revision+1,epoch=epoch+1,active_nonce=NULL,submitted_text=NULL,submitted_draft_revision=NULL,owner_pid=NULL,owner_start=NULL WHERE id=?1 AND active_nonce=?2",params![id,nonce]).map_err(err)?;
        }
        Ok(())
    }

    fn ensure_idle(&self, id: &ConversationId) -> Result<(), String> {
        let active: Option<String> = self
            .db
            .query_row(
                "SELECT active_nonce FROM conversations WHERE id=?1",
                [&id.0],
                |r| r.get(0),
            )
            .map_err(err)?;
        if active.is_some() {
            Err("Conversation has a request in flight".into())
        } else {
            Ok(())
        }
    }

    fn mutate_row(
        &self,
        id: &ConversationId,
        mutation: impl FnOnce(&mut WorkspaceRow),
    ) -> Result<(), String> {
        let mut row = self.row(id)?.ok_or("Conversation not found")?;
        mutation(&mut row);
        row.updated_at_ms = now_ms();
        self.db
            .execute(
                "UPDATE conversations SET row_json=?1,revision=revision+1 WHERE id=?2",
                params![serde_json::to_string(&row).map_err(err)?, id.0],
            )
            .map_err(err)?;
        Ok(())
    }

    fn import(&mut self, namespace: &str, json: &str) -> Result<WorkspaceReply, String> {
        check_text(namespace, 4096, "legacy namespace")?;
        if namespace.trim().is_empty() {
            return Err("Migration needs an application and state-file namespace".into());
        }
        let snapshot = ConversationSnapshot::from_json(json).map_err(err)?;
        let tx = self.db.transaction().map_err(err)?;
        let mut mappings = Vec::new();
        for chat in snapshot.chats() {
            // A restored window may have a new persistence filename. Shared
            // numeric IDs survive that move and must never be re-imported.
            if chat.id() >= SHARED_CHAT_ID_BASE {
                let existing: Option<(String, String)> = tx
                    .query_row(
                        "SELECT id,row_json FROM conversations WHERE numeric_id=?1",
                        [to_i64(chat.id() - SHARED_CHAT_ID_BASE)?],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(err)?;
                if let Some((id, json)) = existing {
                    let row: WorkspaceRow = serde_json::from_str(&json).map_err(err)?;
                    if row.kind == ConversationKind::Api {
                        mappings.push(LegacyChatMapping {
                            legacy_id: chat.id(),
                            id: ConversationId(id),
                        });
                    }
                }
                continue;
            }
            let existing:Option<String>=tx.query_row("SELECT conversation_id FROM legacy_imports WHERE namespace=?1 AND legacy_id=?2",params![namespace,to_i64(chat.id())?],|r|r.get(0)).optional().map_err(err)?;
            let id = if let Some(id) = existing {
                ConversationId(id)
            } else {
                let id = ConversationId(format!("jterm:api:{}", uuid::Uuid::new_v4()));
                let mut row = api_row(id.clone(), chat.title());
                row.archived = chat.archived();
                row.preview = last_preview(chat.turns());
                row.cwd = chat
                    .block_context()
                    .and_then(|c| c.cwd.clone())
                    .unwrap_or_default();
                let one = ConversationSnapshot::from_chats(chat.id(), vec![chat.clone()])
                    .ok_or("Invalid legacy chat")?
                    .to_json()
                    .map_err(err)?;
                tx.execute("INSERT INTO conversations(id,row_json,draft,template,history_truncated) VALUES(?1,?2,?3,?4,?5)",params![id.0,serde_json::to_string(&row).map_err(err)?,chat.draft(),one,chat.history_truncated()]).map_err(err)?;
                append_turns(&tx, &id, chat.turns())?;
                tx.execute("INSERT INTO legacy_imports(namespace,legacy_id,conversation_id) VALUES(?1,?2,?3)",params![namespace,to_i64(chat.id())?,id.0]).map_err(err)?;
                id
            };
            mappings.push(LegacyChatMapping {
                legacy_id: chat.id(),
                id,
            });
        }
        tx.commit().map_err(err)?;
        Ok(WorkspaceReply::Imported { mappings })
    }

    pub fn load(&self, id: &ConversationId) -> Result<StoredChat, String> {
        let (numeric,row_json,revision,draft_revision,epoch,draft,template,truncated):(i64,String,i64,i64,i64,String,String,bool)=self.db.query_row("SELECT numeric_id,row_json,revision,draft_revision,epoch,draft,template,history_truncated FROM conversations WHERE id=?1",[&id.0],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).map_err(err)?;
        let row: WorkspaceRow = serde_json::from_str(&row_json).map_err(err)?;
        if row.kind != ConversationKind::Api {
            return Err(
                "CLI history remains in its provider; open the terminal to continue".into(),
            );
        }
        let messages = self.history(id, None, 100)?;
        let turns: Vec<_> = messages
            .iter()
            .map(|m| Turn {
                role: if m.role == "user" {
                    Role::User
                } else {
                    Role::Assistant
                },
                text: m.text.clone(),
            })
            .collect();
        let count: i64 = self
            .db
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE conversation_id=?1",
                [&id.0],
                |r| r.get(0),
            )
            .map_err(err)?;
        let previous = if template.is_empty() {
            None
        } else {
            Some(ConversationSnapshot::from_json(&template).map_err(err)?)
        };
        let context = previous
            .as_ref()
            .and_then(|s| s.active_chat())
            .and_then(|c| c.block_context());
        let numeric = SHARED_CHAT_ID_BASE + numeric as u64;
        let api_job = self.api_snapshot(id)?;
        let (submitted, submitted_revision): (Option<String>, Option<i64>) = self
            .db
            .query_row(
                "SELECT submitted_text,submitted_draft_revision FROM conversations WHERE id=?1",
                [&id.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(err)?;
        let pending_draft = api_job
            .as_ref()
            .is_some_and(|job| job.state.is_live() && job.epoch == epoch as u64)
            && submitted_revision == Some(draft_revision)
            && submitted.as_deref() == Some(&draft);
        // Hide only the acknowledged, unchanged submitted draft in the view.
        // The database retains it until completion/cleanup; later edits have
        // their own revision and remain visible even if their text is equal.
        let view_draft = if pending_draft { "" } else { &draft };
        let chat = ChatSnapshot::from_completed_history(
            numeric,
            &row.title,
            row.archived,
            &turns,
            context,
            view_draft,
        )
        .with_history_truncated(truncated);
        let has_older = count as usize > chat.turns().len() || chat.history_truncated();
        let snapshot_json = ConversationSnapshot::from_chats(numeric, vec![chat])
            .ok_or("Invalid stored conversation")?
            .to_json()
            .map_err(err)?;
        let read_position=self.db.query_row("SELECT view,position,updated_at_ms FROM read_positions WHERE conversation_id=?1 ORDER BY updated_at_ms DESC LIMIT 1",[&id.0],|r|Ok(ReadPosition{view:r.get(0)?,position:r.get(1)?,updated_at_ms:r.get::<_,i64>(2)? as u64})).optional().map_err(err)?;
        Ok(StoredChat {
            background_api_supported: true,
            api_job: api_job.map(Box::new),
            row,
            revision: revision as u64,
            draft_revision: draft_revision as u64,
            epoch: epoch as u64,
            snapshot_json,
            has_older,
            read_position,
        })
    }

    fn history(
        &self,
        id: &ConversationId,
        before: Option<u64>,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, String> {
        let mut stmt=self.db.prepare("SELECT sequence,role,body FROM messages WHERE conversation_id=?1 AND sequence<?2 ORDER BY sequence DESC LIMIT ?3").map_err(err)?;
        let result = stmt
            .query_map(
                params![
                    id.0,
                    before.map(to_i64).transpose()?.unwrap_or(i64::MAX),
                    limit.min(100) as i64
                ],
                |r| {
                    Ok(StoredMessage {
                        sequence: r.get::<_, i64>(0)? as u64,
                        role: r.get(1)?,
                        text: r.get(2)?,
                    })
                },
            )
            .map_err(err)?;
        let mut messages = Vec::new();
        let mut bytes = 0;
        let mut assistant = None;
        for message in result {
            let message = message.map_err(err)?;
            if message.role == "assistant" {
                assistant = Some(message);
                continue;
            }
            let Some(answer) = assistant.take() else {
                continue;
            };
            let pair_bytes = message.text.len() + answer.text.len();
            if !messages.is_empty() && bytes + pair_bytes > 1024 * 1024 {
                break;
            }
            bytes += pair_bytes;
            messages.push(answer);
            messages.push(message);
        }
        messages.reverse();
        Ok(messages)
    }

    fn save(
        &mut self,
        id: &ConversationId,
        expected: u64,
        json: &str,
    ) -> Result<WorkspaceReply, String> {
        let incoming = ConversationSnapshot::from_json(json).map_err(err)?;
        if incoming.chats().len() != 1 {
            return Err("SaveChat requires exactly one conversation".into());
        }
        let chat = &incoming.chats()[0];
        let current = self.load(id)?;
        let expected_numeric = self
            .db
            .query_row(
                "SELECT numeric_id FROM conversations WHERE id=?1",
                [&id.0],
                |r| r.get::<_, u64>(0),
            )
            .map_err(err)?
            + SHARED_CHAT_ID_BASE;
        if chat.id() != expected_numeric {
            return Err("SaveChat snapshot belongs to a different conversation".into());
        }
        if current.revision != expected {
            return Err("Conversation revision conflict: reload before saving; local changes were not overwritten".into());
        }
        self.ensure_idle(id)?;
        let old = self.history(id, None, 100)?;
        let old_turns: Vec<_> = old
            .iter()
            .map(|m| Turn {
                role: if m.role == "user" {
                    Role::User
                } else {
                    Role::Assistant
                },
                text: m.text.clone(),
            })
            .collect();
        let overlap = overlap_pairs(&old_turns, chat.turns())?;
        let mut row = current.row;
        row.title = chat.title().into();
        row.archived = chat.archived();
        row.preview = last_preview(chat.turns());
        row.updated_at_ms = now_ms();
        row.cwd = chat
            .block_context()
            .and_then(|c| c.cwd.clone())
            .unwrap_or_default();
        let tx = self.db.transaction().map_err(err)?;
        let updated=tx.execute("UPDATE conversations SET row_json=?1,revision=revision+1,draft_revision=draft_revision+CASE WHEN draft<>?2 THEN 1 ELSE 0 END,draft=?2,template=?3,history_truncated=history_truncated OR ?4 WHERE id=?5 AND revision=?6 AND active_nonce IS NULL",params![serde_json::to_string(&row).map_err(err)?,chat.draft(),json,chat.history_truncated(),id.0,to_i64(expected)?]).map_err(err)?;
        if updated != 1 {
            return Err("Conversation revision changed during save".into());
        }
        append_turns(&tx, id, &chat.turns()[overlap..])?;
        tx.commit().map_err(err)?;
        Ok(WorkspaceReply::Chat {
            chat: self.load(id)?,
        })
    }

    fn begin(
        &mut self,
        id: &ConversationId,
        revision: u64,
        draft_revision: u64,
        text: &str,
        owner: (u32, String),
    ) -> Result<WorkspaceReply, String> {
        check_text(text, MAX_DRAFT, "message")?;
        if text.trim().is_empty() {
            return Err("Message cannot be empty".into());
        }
        let current = self.load(id)?;
        if current.row.archived {
            return Err("Unarchive this conversation before sending".into());
        }
        if current.revision != revision || current.draft_revision != draft_revision {
            return Err(
                "Submission revision conflict: the exact draft has not been acknowledged".into(),
            );
        }
        let nonce = uuid::Uuid::new_v4().to_string();
        if !process_matches(owner.0, &owner.1) {
            return Err("Submitting frontend exited before the request was accepted".into());
        }
        let mut row = current.row;
        if row.title == crate::ai::DEFAULT_CHAT_TITLE {
            row.title = ChatSnapshot::from_completed_history(1, text, false, &[], None, "")
                .title()
                .chars()
                .take(52)
                .collect();
        }
        row.updated_at_ms = now_ms();
        let changed=self.db.execute("UPDATE conversations SET epoch=epoch+1,revision=revision+1,active_nonce=?1,submitted_text=?2,submitted_draft_revision=?3,owner_pid=?6,owner_start=?7,row_json=?8 WHERE id=?4 AND revision=?5 AND draft_revision=?3 AND active_nonce IS NULL",params![nonce,text,to_i64(draft_revision)?,id.0,to_i64(revision)?,owner.0,owner.1,serde_json::to_string(&row).map_err(err)?]).map_err(err)?;
        if changed != 1 {
            return Err("Conversation already has a request in flight".into());
        }
        Ok(WorkspaceReply::Submission {
            receipt: SubmissionReceipt {
                id: id.clone(),
                epoch: current.epoch + 1,
                nonce,
                draft_revision,
            },
        })
    }

    fn complete(
        &mut self,
        receipt: &SubmissionReceipt,
        text: &str,
    ) -> Result<WorkspaceReply, String> {
        let tx = self.db.transaction().map_err(err)?;
        complete_receipt(&tx, receipt, text)?;
        tx.commit().map_err(err)?;
        Ok(WorkspaceReply::Chat {
            chat: self.load(&receipt.id)?,
        })
    }
    fn cancel(
        &mut self,
        receipt: &SubmissionReceipt,
        restore: bool,
    ) -> Result<WorkspaceReply, String> {
        cancel_receipt(&self.db, receipt, restore)?;
        Ok(WorkspaceReply::Chat {
            chat: self.load(&receipt.id)?,
        })
    }
}

fn complete_receipt(
    db: &Connection,
    receipt: &SubmissionReceipt,
    text: &str,
) -> Result<(), String> {
    check_text(text, MAX_MESSAGE, "assistant message")?;
    let user:Option<String>=db.query_row("SELECT submitted_text FROM conversations WHERE id=?1 AND epoch=?2 AND active_nonce=?3 AND submitted_draft_revision=?4",params![receipt.id.0,to_i64(receipt.epoch)?,receipt.nonce,to_i64(receipt.draft_revision)?],|r|r.get(0)).optional().map_err(err)?.flatten();
    let user = user.ok_or("Stale submission receipt: reply discarded")?;
    append_turns(
        db,
        &receipt.id,
        &[
            Turn {
                role: Role::User,
                text: user,
            },
            Turn {
                role: Role::Assistant,
                text: text.into(),
            },
        ],
    )?;
    let row_json: String = db
        .query_row(
            "SELECT row_json FROM conversations WHERE id=?1",
            [&receipt.id.0],
            |r| r.get(0),
        )
        .map_err(err)?;
    let mut row: WorkspaceRow = serde_json::from_str(&row_json).map_err(err)?;
    row.preview = preview(text);
    row.updated_at_ms = now_ms();
    db.execute("UPDATE conversations SET row_json=?1,revision=revision+1,draft=CASE WHEN draft_revision=?2 AND draft=submitted_text THEN '' ELSE draft END,draft_revision=draft_revision+CASE WHEN draft_revision=?2 AND draft=submitted_text THEN 1 ELSE 0 END,active_nonce=NULL,submitted_text=NULL,submitted_draft_revision=NULL,owner_pid=NULL,owner_start=NULL WHERE id=?3 AND epoch=?4 AND active_nonce=?5 AND submitted_draft_revision=?2",params![serde_json::to_string(&row).map_err(err)?,to_i64(receipt.draft_revision)?,receipt.id.0,to_i64(receipt.epoch)?,receipt.nonce]).map_err(err)?;
    Ok(())
}
fn cancel_receipt(
    db: &Connection,
    receipt: &SubmissionReceipt,
    restore: bool,
) -> Result<(), String> {
    let changed=db.execute("UPDATE conversations SET draft=CASE WHEN ?1 AND draft_revision=?2 AND (draft='' OR draft=submitted_text) THEN submitted_text ELSE draft END,draft_revision=draft_revision+CASE WHEN ?1 AND draft_revision=?2 AND (draft='' OR draft=submitted_text) THEN 1 ELSE 0 END,revision=revision+1,epoch=epoch+1,active_nonce=NULL,submitted_text=NULL,submitted_draft_revision=NULL,owner_pid=NULL,owner_start=NULL WHERE id=?3 AND epoch=?4 AND active_nonce=?5 AND submitted_draft_revision=?2",params![restore,to_i64(receipt.draft_revision)?,receipt.id.0,to_i64(receipt.epoch)?,receipt.nonce]).map_err(err)?;
    if changed != 1 {
        return Err("Stale submission receipt: cancellation ignored".into());
    }
    Ok(())
}

fn api_row(id: ConversationId, title: &str) -> WorkspaceRow {
    WorkspaceRow {
        id,
        kind: ConversationKind::Api,
        title: if title.trim().is_empty() {
            "New chat".into()
        } else {
            title.trim().into()
        },
        provider: "API".into(),
        cwd: String::new(),
        preview: String::new(),
        updated_at_ms: now_ms(),
        archived: false,
        pinned: false,
        tags: Vec::new(),
        runtime: RuntimeState::Idle,
    }
}
fn append_turns(db: &Connection, id: &ConversationId, turns: &[Turn]) -> Result<(), String> {
    for turn in turns {
        check_text(&turn.text, MAX_MESSAGE, "message")?;
        db.execute(
            "INSERT INTO messages(conversation_id,role,body) VALUES(?1,?2,?3)",
            params![
                id.0,
                if turn.role == Role::User {
                    "user"
                } else {
                    "assistant"
                },
                turn.text
            ],
        )
        .map_err(err)?;
    }
    Ok(())
}
fn overlap_pairs(old: &[Turn], new: &[Turn]) -> Result<usize, String> {
    if old.is_empty() {
        return Ok(0);
    }
    if new.is_empty() {
        return Err("Saving an empty active window cannot erase durable history".into());
    }
    for count in (2..=old.len().min(new.len())).rev().filter(|n| n % 2 == 0) {
        if old[old.len() - count..] == new[..count] {
            return Ok(count);
        }
    }
    Err("History conflict: active window does not overlap durable history; reload before continuing".into())
}
fn check_text(text: &str, limit: usize, label: &str) -> Result<(), String> {
    if text.len() > limit || text.contains('\0') {
        Err(format!("Invalid or oversized {label}"))
    } else {
        Ok(())
    }
}
fn to_i64(n: u64) -> Result<i64, String> {
    i64::try_from(n).map_err(|_| "Revision/ID exceeds the supported range".into())
}
fn preview(text: &str) -> String {
    crate::redact::redact_secrets(text)
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(160)
        .collect::<String>()
        .replace('\n', " ")
}
fn last_preview(turns: &[Turn]) -> String {
    turns.last().map(|t| preview(&t.text)).unwrap_or_default()
}
fn validate_tags(tags: &[String]) -> Result<(), String> {
    if tags.len() > 16 {
        return Err("A conversation supports at most 16 tags".into());
    }
    for tag in tags {
        check_text(tag, 64, "tag")?;
        if tag.trim().is_empty() || tag.chars().any(char::is_control) {
            return Err("Tags must contain visible text".into());
        }
    }
    Ok(())
}
fn validate_index_row(row: &WorkspaceRow) -> Result<(), String> {
    let valid_kind = match row.kind {
        ConversationKind::Cli => {
            row.id.0.starts_with("jterm:cli:v1:")
                && matches!(row.provider.as_str(), "Codex" | "Claude")
        }
        ConversationKind::Task => {
            row.id.0.starts_with("jterm:task:v1:")
                && !row.provider.is_empty()
                && row.provider.len() <= 64
        }
        ConversationKind::Api => false,
    };
    if !valid_kind || row.id.0.len() > 32768 {
        return Err("Invalid source-qualified session/task index row".into());
    }
    check_text(&row.title, 512, "session title")?;
    check_text(&row.cwd, 8192, "project path")?;
    check_text(&row.preview, 4096, "session preview")?;
    validate_tags(&row.tags)
}
fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn store() -> (Store, Paths) {
        let p =
            Paths::at(std::env::temp_dir().join(format!("jterm-store-{}", uuid::Uuid::new_v4())))
                .unwrap();
        (Store::open(&p).unwrap(), p)
    }
    fn create(s: &mut Store) -> StoredChat {
        match s
            .command(WorkspaceCommand::CreateChat {
                title: "Test".into(),
            })
            .unwrap()
        {
            WorkspaceReply::Chat { chat } => chat,
            _ => panic!(),
        }
    }
    fn receipt(s: &mut Store, c: &StoredChat) -> SubmissionReceipt {
        match s
            .command(WorkspaceCommand::BeginSubmission {
                id: c.row.id.clone(),
                expected_revision: c.revision,
                draft_revision: c.draft_revision,
                text: "question".into(),
            })
            .unwrap()
        {
            WorkspaceReply::Submission { receipt } => receipt,
            _ => panic!(),
        }
    }
    #[test]
    fn exact_receipt_rejects_cancelled_epoch_and_preserves_newer_draft() {
        let (mut s, p) = store();
        let c = create(&mut s);
        let r = receipt(&mut s, &c);
        s.command(WorkspaceCommand::SetDraft {
            id: c.row.id.clone(),
            expected_revision: 0,
            text: "next question".into(),
        })
        .unwrap();
        s.command(WorkspaceCommand::CompleteSubmission {
            receipt: r.clone(),
            text: "answer".into(),
        })
        .unwrap();
        assert!(s
            .command(WorkspaceCommand::CompleteSubmission {
                receipt: r,
                text: "late duplicate".into()
            })
            .is_err());
        let c = s.load(&c.row.id).unwrap();
        let snap = ConversationSnapshot::from_json(&c.snapshot_json).unwrap();
        assert_eq!(snap.active_chat().unwrap().draft(), "next question");
        assert_eq!(snap.turns().len(), 2);
        let r = receipt(&mut s, &c);
        s.command(WorkspaceCommand::CancelSubmission {
            receipt: r.clone(),
            restore_draft: true,
        })
        .unwrap();
        assert!(s
            .command(WorkspaceCommand::CompleteSubmission {
                receipt: r,
                text: "late".into()
            })
            .is_err());
        drop(s);
        std::fs::remove_dir_all(p.state).unwrap();
    }
    #[test]
    fn legacy_ids_are_namespaced_and_import_is_idempotent() {
        if super::super::isolated_lease_test(
            "agent_workspace::store::tests::legacy_ids_are_namespaced_and_import_is_idempotent",
        ) {
            return;
        }
        let (mut s, p) = store();
        let snapshot = ConversationSnapshot::from_completed_history(
            &[
                Turn {
                    role: Role::User,
                    text: "q".into(),
                },
                Turn {
                    role: Role::Assistant,
                    text: "a".into(),
                },
            ],
            None,
        )
        .unwrap();
        let json = snapshot.to_json().unwrap();
        for namespace in ["anvil/state", "anvil/state", "forge/state"] {
            s.import(namespace, &json).unwrap();
        }
        assert_eq!(s.rows().unwrap().len(), 2);
        drop(s);
        let s = Store::open(&p).unwrap();
        assert_eq!(s.rows().unwrap().len(), 2);
        drop(s);
        std::fs::remove_dir_all(p.state).unwrap();
    }
    #[test]
    fn durable_history_exceeds_active_window_and_pages() {
        let (mut s, p) = store();
        let mut c = create(&mut s);
        for i in 0..65 {
            let r = receipt(&mut s, &c);
            s.command(WorkspaceCommand::CompleteSubmission {
                receipt: r,
                text: format!("answer {i}"),
            })
            .unwrap();
            c = s.load(&c.row.id).unwrap();
        }
        assert!(c.has_older);
        assert_eq!(
            ConversationSnapshot::from_json(&c.snapshot_json)
                .unwrap()
                .turns()
                .len(),
            100
        );
        let recent = s.history(&c.row.id, None, 100).unwrap();
        let older = s.history(&c.row.id, Some(recent[0].sequence), 100).unwrap();
        assert_eq!(older.len(), 30);
        let first = recent[0].sequence;
        let json = c.snapshot_json.clone();
        s.save(&c.row.id, c.revision, &json).unwrap();
        assert_eq!(s.history(&c.row.id, None, 100).unwrap()[0].sequence, first);
        drop(s);
        std::fs::remove_dir_all(p.state).unwrap();
    }
    #[test]
    fn wrong_receipt_draft_and_wrong_conversation_snapshot_are_rejected() {
        let (mut s, p) = store();
        let c = create(&mut s);
        let mut r = receipt(&mut s, &c);
        r.draft_revision += 1;
        assert!(s
            .command(WorkspaceCommand::CompleteSubmission {
                receipt: r.clone(),
                text: "bad".into()
            })
            .is_err());
        assert!(s
            .command(WorkspaceCommand::CancelSubmission {
                receipt: r,
                restore_draft: true
            })
            .is_err());
        let second = create(&mut s);
        assert!(s
            .save(&second.row.id, second.revision, &c.snapshot_json)
            .is_err());
        drop(s);
        std::fs::remove_dir_all(p.state).unwrap();
    }
    #[test]
    fn abandoned_owner_revokes_epoch_without_overwriting_newer_draft() {
        let (mut s, p) = store();
        let c = create(&mut s);
        let r = receipt(&mut s, &c);
        s.command(WorkspaceCommand::SetDraft {
            id: c.row.id.clone(),
            expected_revision: 0,
            text: "newer".into(),
        })
        .unwrap();
        s.db.execute(
            "UPDATE conversations SET owner_start='stale-process-identity' WHERE id=?1",
            [&c.row.id.0],
        )
        .unwrap();
        s.reap_abandoned_requests().unwrap();
        let c = s.load(&c.row.id).unwrap();
        assert!(c.epoch > r.epoch);
        assert_eq!(
            ConversationSnapshot::from_json(&c.snapshot_json)
                .unwrap()
                .active_chat()
                .unwrap()
                .draft(),
            "newer"
        );
        assert!(s
            .command(WorkspaceCommand::CompleteSubmission {
                receipt: r,
                text: "late".into()
            })
            .is_err());
        assert!(matches!(
            s.command(WorkspaceCommand::BeginSubmission {
                id: c.row.id.clone(),
                expected_revision: c.revision,
                draft_revision: c.draft_revision,
                text: "next".into()
            }),
            Ok(WorkspaceReply::Submission { .. })
        ));
        drop(s);
        std::fs::remove_dir_all(p.state).unwrap();
    }
    #[test]
    fn global_chat_restore_does_not_reimport_under_a_new_window_path() {
        let (mut s, p) = store();
        let c = create(&mut s);
        s.import("anvil/old-window", &c.snapshot_json).unwrap();
        s.import("anvil/new-window", &c.snapshot_json).unwrap();
        assert_eq!(s.rows().unwrap().len(), 1);
        s.command(WorkspaceCommand::SetReadPosition {
            id: c.row.id.clone(),
            view: "chat".into(),
            position: 123.5,
        })
        .unwrap();
        assert_eq!(
            s.load(&c.row.id).unwrap().read_position.unwrap().position,
            123.5
        );
        assert!(Store::open(&p).is_err());
        drop(s);
        std::fs::remove_dir_all(p.state).unwrap();
    }
    #[test]
    fn history_page_of_one_returns_a_complete_pair_and_honest_end_cursor() {
        let (mut s, p) = store();
        let c = create(&mut s);
        let r = receipt(&mut s, &c);
        s.command(WorkspaceCommand::CompleteSubmission {
            receipt: r,
            text: "answer".into(),
        })
        .unwrap();
        match s
            .command(WorkspaceCommand::History {
                id: c.row.id,
                before: None,
                limit: 1,
            })
            .unwrap()
        {
            WorkspaceReply::Messages {
                messages,
                next_before,
            } => {
                assert_eq!(messages.len(), 2);
                assert_eq!(next_before, None);
            }
            _ => panic!(),
        }
        drop(s);
        std::fs::remove_dir_all(p.state).unwrap();
    }

    #[test]
    fn block_seeded_request_never_consumes_an_unrelated_composer_draft() {
        let (mut s, p) = store();
        let c = create(&mut s);
        s.command(WorkspaceCommand::SetDraft {
            id: c.row.id.clone(),
            expected_revision: 0,
            text: "unsent composer".into(),
        })
        .unwrap();
        let c = s.load(&c.row.id).unwrap();
        let r = receipt(&mut s, &c);
        s.command(WorkspaceCommand::CompleteSubmission {
            receipt: r,
            text: "block explanation".into(),
        })
        .unwrap();
        let c = s.load(&c.row.id).unwrap();
        assert_eq!(
            ConversationSnapshot::from_json(&c.snapshot_json)
                .unwrap()
                .active_chat()
                .unwrap()
                .draft(),
            "unsent composer"
        );
        let r = receipt(&mut s, &c);
        s.command(WorkspaceCommand::CancelSubmission {
            receipt: r,
            restore_draft: true,
        })
        .unwrap();
        let c = s.load(&c.row.id).unwrap();
        assert_eq!(
            ConversationSnapshot::from_json(&c.snapshot_json)
                .unwrap()
                .active_chat()
                .unwrap()
                .draft(),
            "unsent composer"
        );
        let _ = receipt(&mut s, &c);
        s.db.execute(
            "UPDATE conversations SET owner_start='dead' WHERE id=?1",
            [&c.row.id.0],
        )
        .unwrap();
        s.reap_abandoned_requests().unwrap();
        let c = s.load(&c.row.id).unwrap();
        assert_eq!(
            ConversationSnapshot::from_json(&c.snapshot_json)
                .unwrap()
                .active_chat()
                .unwrap()
                .draft(),
            "unsent composer"
        );
        drop(s);
        std::fs::remove_dir_all(p.state).unwrap();
    }
    #[test]
    fn cli_organization_is_source_qualified_and_survives_reindex() {
        let (mut s, p) = store();
        let mut a = api_row(
            source_id(
                "Codex",
                "host",
                std::path::Path::new("/profile-a"),
                "same-id",
            )
            .unwrap(),
            "Provider title",
        );
        a.kind = ConversationKind::Cli;
        a.provider = "Codex".into();
        let mut b = a.clone();
        b.id = source_id(
            "Codex",
            "host",
            std::path::Path::new("/profile-b"),
            "same-id",
        )
        .unwrap();
        s.command(WorkspaceCommand::IndexSessions {
            sessions: vec![a.clone(), b.clone()],
        })
        .unwrap();
        s.command(WorkspaceCommand::Rename {
            id: a.id.clone(),
            title: "Local alias".into(),
        })
        .unwrap();
        s.command(WorkspaceCommand::Pin {
            id: a.id.clone(),
            pinned: true,
        })
        .unwrap();
        s.command(WorkspaceCommand::Tags {
            id: a.id.clone(),
            tags: vec!["Review".into()],
        })
        .unwrap();
        s.command(WorkspaceCommand::IndexSessions {
            sessions: vec![a.clone(), b.clone()],
        })
        .unwrap();
        let changed = s.row(&a.id).unwrap().unwrap();
        let other = s.row(&b.id).unwrap().unwrap();
        assert_eq!(changed.title, "Local alias");
        assert!(changed.pinned);
        assert_eq!(changed.tags, ["Review"]);
        assert_eq!(other.title, "Provider title");
        assert!(!other.pinned);
        assert!(other.tags.is_empty());
        drop(s);
        std::fs::remove_dir_all(p.state).unwrap();
    }
}
