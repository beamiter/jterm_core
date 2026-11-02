//! Bounded incremental indexing. The index is evidence, never execution state.
use super::*;
use crate::agent_workspace::search::*;
use crate::agent_workspace::WorkspaceQuery;

const BACKFILL_BATCH: usize = 64;
const SEARCH_SCAN_LIMIT: usize = 4096;

pub(super) fn migrate(db: &Connection, old_version: i64) -> Result<(), String> {
    db.execute_batch("BEGIN;
        CREATE TABLE IF NOT EXISTS workspace_text (
            document_id INTEGER PRIMARY KEY AUTOINCREMENT,
            conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
            event_id TEXT NOT NULL, revision TEXT NOT NULL, source TEXT NOT NULL,
            message_sequence INTEGER, body TEXT NOT NULL, truncated INTEGER NOT NULL DEFAULT 0,
            UNIQUE(conversation_id,event_id));
        CREATE INDEX IF NOT EXISTS workspace_text_conversation ON workspace_text(conversation_id,document_id);
        CREATE UNIQUE INDEX IF NOT EXISTS workspace_text_message ON workspace_text(message_sequence) WHERE message_sequence IS NOT NULL;
        CREATE VIRTUAL TABLE IF NOT EXISTS workspace_text_fts USING fts5(body,content='workspace_text',content_rowid='document_id',tokenize='trigram');
        CREATE TRIGGER IF NOT EXISTS workspace_text_insert AFTER INSERT ON workspace_text BEGIN
            INSERT INTO workspace_text_fts(rowid,body) VALUES(new.document_id,new.body); END;
        CREATE TRIGGER IF NOT EXISTS workspace_text_delete AFTER DELETE ON workspace_text BEGIN
            INSERT INTO workspace_text_fts(workspace_text_fts,rowid,body) VALUES('delete',old.document_id,old.body); END;
        CREATE TRIGGER IF NOT EXISTS workspace_text_update AFTER UPDATE OF body ON workspace_text BEGIN
            INSERT INTO workspace_text_fts(workspace_text_fts,rowid,body) VALUES('delete',old.document_id,old.body);
            INSERT INTO workspace_text_fts(rowid,body) VALUES(new.document_id,new.body); END;
        CREATE TABLE IF NOT EXISTS workspace_index_pending (
            conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE);
        CREATE TABLE IF NOT EXISTS workspace_message_pending (
            sequence INTEGER PRIMARY KEY REFERENCES messages(sequence) ON DELETE CASCADE);
        CREATE TRIGGER IF NOT EXISTS workspace_message_new AFTER INSERT ON messages BEGIN
            INSERT OR IGNORE INTO workspace_message_pending VALUES(new.sequence); END;
        DROP TRIGGER IF EXISTS workspace_index_new;
        DROP TRIGGER IF EXISTS workspace_index_changed;
        CREATE TRIGGER workspace_index_new AFTER INSERT ON conversations BEGIN
            INSERT INTO workspace_index_pending SELECT new.id WHERE NOT EXISTS(SELECT 1 FROM workspace_index_pending WHERE conversation_id=new.id); END;
        CREATE TRIGGER workspace_index_changed AFTER UPDATE OF row_json ON conversations
            WHEN json_extract(old.row_json,'$.title') IS NOT json_extract(new.row_json,'$.title')
              OR json_extract(old.row_json,'$.preview') IS NOT json_extract(new.row_json,'$.preview')
              OR json_extract(old.row_json,'$.cwd') IS NOT json_extract(new.row_json,'$.cwd')
              OR json_extract(old.row_json,'$.provider') IS NOT json_extract(new.row_json,'$.provider')
              OR json_extract(old.row_json,'$.facts.branch') IS NOT json_extract(new.row_json,'$.facts.branch')
              OR json_extract(old.row_json,'$.tags') IS NOT json_extract(new.row_json,'$.tags') BEGIN
            INSERT INTO workspace_index_pending SELECT new.id WHERE NOT EXISTS(SELECT 1 FROM workspace_index_pending WHERE conversation_id=new.id); END;
        CREATE TRIGGER IF NOT EXISTS workspace_message_delete AFTER DELETE ON messages BEGIN
            DELETE FROM workspace_text WHERE message_sequence=old.sequence; END;
        UPDATE schema_version SET version=5; COMMIT;").map_err(err)?;
    if old_version < 5 {
        db.execute(
            "INSERT OR IGNORE INTO workspace_index_pending SELECT id FROM conversations",
            [],
        )
        .map_err(err)?;
        db.execute(
            "INSERT OR IGNORE INTO workspace_message_pending SELECT sequence FROM messages",
            [],
        )
        .map_err(err)?;
    }
    Ok(())
}

/// Stable content revision, only an equality marker; never a permission token.
fn revision(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("text-sha256:{:x}", Sha256::digest(text.as_bytes()))
}

fn bounded_text(text: &str) -> (String, bool) {
    let safe = crate::redact::redact_secrets(text);
    let safe: String = safe
        .chars()
        .filter(|ch| {
            (!ch.is_control() || matches!(*ch, '\n' | '\t'))
                && (ch.is_control() || !crate::review_input::is_visual_spoofing_character(*ch))
        })
        .collect();
    let mut end = safe.len().min(WORKSPACE_TEXT_BYTES);
    while !safe.is_char_boundary(end) {
        end -= 1;
    }
    (safe[..end].to_owned(), end < safe.len())
}

fn put_document(db: &Connection, document: &WorkspaceTextDocument) -> Result<(), String> {
    check_text(&document.event_id, 512, "search event identity")?;
    check_text(&document.revision, 256, "search revision")?;
    if document.event_id.is_empty() || document.revision.is_empty() {
        return Err("Search records require stable event and revision identities".into());
    }
    let (body, clipped) = bounded_text(&document.text);
    let source = serde_json::to_string(&document.source).map_err(err)?;
    let sequence = document.message_sequence.map(to_i64).transpose()?;
    let truncated = clipped || document.truncated;
    let previous: Option<(String, String, String, Option<i64>, bool)> = db.query_row(
        "SELECT revision,body,source,message_sequence,truncated FROM workspace_text WHERE conversation_id=?1 AND event_id=?2",
        params![document.conversation.0, document.event_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))).optional().map_err(err)?;
    if let Some((version, old_body, old_source, old_sequence, old_truncated)) = previous {
        if version == document.revision {
            if old_body != body
                || old_source != source
                || old_sequence != sequence
                || old_truncated != truncated
            {
                return Err("An indexed revision is immutable. Provide a new source revision when its evidence changes.".into());
            }
            return Ok(());
        }
    }
    db.execute("INSERT INTO workspace_text(conversation_id,event_id,revision,source,message_sequence,body,truncated)
        VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(conversation_id,event_id) DO UPDATE SET
        revision=excluded.revision,source=excluded.source,message_sequence=excluded.message_sequence,body=excluded.body,truncated=excluded.truncated
        WHERE workspace_text.revision<>excluded.revision",
        params![document.conversation.0, document.event_id, document.revision, source,
            sequence, body, truncated]).map_err(err)?;
    Ok(())
}

pub(super) fn index_api_message(
    db: &Connection,
    id: &ConversationId,
    sequence: i64,
    body: &str,
) -> Result<(), String> {
    put_document(
        db,
        &WorkspaceTextDocument {
            conversation: id.clone(),
            event_id: format!("message:{sequence}"),
            revision: format!("message-v1:{sequence}"),
            source: SearchSource::ApiMessage,
            message_sequence: Some(u64::try_from(sequence).map_err(err)?),
            text: body.to_owned(),
            truncated: false,
        },
    )?;
    db.execute(
        "DELETE FROM workspace_message_pending WHERE sequence=?1",
        [sequence],
    )
    .map_err(err)?;
    Ok(())
}

impl Store {
    pub(super) fn index_native_text(
        &self,
        snapshot: &crate::agent_workspace::NativeSnapshot,
    ) -> Result<(), String> {
        let write = |event_id: String, source, text: String, truncated| {
            let (text, clipped) = bounded_text(&text);
            put_document(
                &self.db,
                &WorkspaceTextDocument {
                    conversation: snapshot.id.clone(),
                    event_id,
                    revision: format!("{}:{}", revision(&text), u8::from(truncated || clipped)),
                    source,
                    message_sequence: None,
                    text,
                    truncated: truncated || clipped,
                },
            )
        };
        if let Some(brief) = &snapshot.task.brief {
            write(
                "task:goal".into(),
                SearchSource::NativeTurn,
                brief.goal.clone(),
                false,
            )?;
        }
        if let Some(view) = &snapshot.view {
            for turn in view.persisted_history.iter() {
                write(
                    format!("provider:{}", turn.provider_turn_id),
                    SearchSource::ProviderHistory,
                    format!("User\n{}\n\nAssistant\n{}", turn.user_text, turn.agent_text),
                    false,
                )?;
            }
            for turn in view.turn_history.iter() {
                write(
                    format!("turn:{}", turn.local_turn_id),
                    SearchSource::NativeTurn,
                    format!(
                        "User\n{}\n\nAssistant\n{}",
                        turn.follow_up_feedback
                            .as_deref()
                            .unwrap_or("(Initial task goal is stored separately)"),
                        turn.agent_text
                    ),
                    turn.agent_text_truncated || turn.dropped_updates > 0,
                )?;
            }
            // Streaming deltas remain in the live view; only a settled turn
            // changes durable search evidence and its revision.
            if !snapshot.live && !snapshot.preparing {
                if let Some(turn_id) = view.displayed_turn_id {
                    write(
                        format!("turn:{turn_id}"),
                        SearchSource::NativeTurn,
                        format!(
                            "User\n{}\n\nAssistant\n{}",
                            view.displayed_follow_up_feedback
                                .as_deref()
                                .unwrap_or("(Initial task goal is stored separately)"),
                            view.agent_text
                        ),
                        view.agent_text_truncated || view.dropped_updates > 0,
                    )?;
                }
            }
        }
        if let Some(validation) = &snapshot.validation {
            if !validation.preparing
                && snapshot.task.validation.status
                    != crate::agent_task::TaskValidationStatus::Running
            {
                write(
                    format!("validation:{}", validation.run_id),
                    SearchSource::Validation,
                    format!(
                        "Attempt: {}\nStatus: {:?}\nCommand: {}\nExit: {:?}\n\n{}",
                        snapshot.task.validation.attempt,
                        snapshot.task.validation.status,
                        snapshot.task.validation_command().unwrap_or("(none)"),
                        snapshot.task.validation.exit_code,
                        validation.output
                    ),
                    validation.output_truncated,
                )?;
            }
        }
        Ok(())
    }

    pub(in crate::agent_workspace) fn advance_search_index(&self) -> Result<(), String> {
        self.db.execute_batch("BEGIN IMMEDIATE").map_err(err)?;
        let result = self.backfill_search();
        if result.is_ok() {
            self.db.execute_batch("COMMIT").map_err(err)?;
        } else {
            let _ = self.db.execute_batch("ROLLBACK");
        }
        result
    }

    fn backfill_search(&self) -> Result<(), String> {
        let metadata: Vec<String> = self.db.prepare("SELECT p.conversation_id FROM workspace_index_pending p JOIN conversations c ON c.id=p.conversation_id ORDER BY c.numeric_id DESC LIMIT ?1").map_err(err)?
            .query_map([BACKFILL_BATCH as i64], |r| r.get(0)).map_err(err)?.collect::<Result<_,_>>().map_err(err)?;
        for id in metadata {
            if let Some(row) = self.row(&ConversationId(id.clone()))? {
                let text = format!(
                    "{}\n{}\n{}\n{}\n{}\n{}\n{}",
                    row.title,
                    row.id.0,
                    row.provider,
                    row.cwd,
                    row.facts.branch.as_deref().unwrap_or(""),
                    row.tags.join(" "),
                    row.preview
                );
                let (text, truncated) = bounded_text(&text);
                put_document(
                    &self.db,
                    &WorkspaceTextDocument {
                        conversation: row.id,
                        event_id: "metadata".into(),
                        revision: format!("{}:{}", revision(&text), u8::from(truncated)),
                        source: SearchSource::Metadata,
                        message_sequence: None,
                        text,
                        truncated,
                    },
                )?;
            }
            self.db
                .execute(
                    "DELETE FROM workspace_index_pending WHERE conversation_id=?1",
                    [id],
                )
                .map_err(err)?;
        }
        let messages: Vec<(i64, String, String)> = self.db.prepare("SELECT m.sequence,m.conversation_id,m.body FROM workspace_message_pending p JOIN messages m ON m.sequence=p.sequence ORDER BY p.sequence LIMIT ?1").map_err(err)?
            .query_map([BACKFILL_BATCH as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map_err(err)?.collect::<Result<_,_>>().map_err(err)?;
        for (sequence, id, text) in messages {
            index_api_message(&self.db, &ConversationId(id), sequence, &text)?;
        }
        Ok(())
    }

    pub(in crate::agent_workspace) fn catalog_page(
        &self,
        before: Option<u64>,
        limit: usize,
    ) -> Result<WorkspaceReply, String> {
        let limit = limit.clamp(1, 256);
        let cursor = before.map(to_i64).transpose()?.unwrap_or(i64::MAX);
        let records: Vec<(i64, String)> = self.db.prepare("SELECT numeric_id,row_json FROM conversations WHERE numeric_id<?1 ORDER BY numeric_id DESC LIMIT ?2").map_err(err)?
            .query_map(params![cursor, limit as i64 + 1], |r| Ok((r.get(0)?, r.get(1)?))).map_err(err)?.collect::<Result<_,_>>().map_err(err)?;
        let has_more = records.len() > limit;
        let mut rows = Vec::new();
        let mut next_before = None;
        for (numeric, json) in records.into_iter().take(limit) {
            let mut row = serde_json::from_str(&json).map_err(err)?;
            self.hydrate_row(&mut row)?;
            rows.push(row);
            next_before = Some(numeric as u64);
        }
        let total: i64 = self
            .db
            .query_row("SELECT count(*) FROM conversations", [], |r| r.get(0))
            .map_err(err)?;
        Ok(WorkspaceReply::CatalogPage {
            rows,
            next_before: has_more.then_some(next_before).flatten(),
            total: total as usize,
        })
    }

    pub(super) fn hydrate_row(&self, row: &mut WorkspaceRow) -> Result<(), String> {
        if row.kind == ConversationKind::Api {
            let active: Option<String> = self
                .db
                .query_row(
                    "SELECT active_nonce FROM conversations WHERE id=?1",
                    [&row.id.0],
                    |r| r.get(0),
                )
                .map_err(err)?;
            if let Some(snapshot) = self.api_snapshot(&row.id)? {
                row.apply_api_snapshot(&snapshot)?;
                // An unacknowledged legacy writer is not the settled host job.
                let hosted_nonce: String = self
                    .db
                    .query_row("SELECT nonce FROM api_jobs WHERE id=?1", [&row.id.0], |r| {
                        r.get(0)
                    })
                    .map_err(err)?;
                if active
                    .as_deref()
                    .is_some_and(|nonce| nonce != hosted_nonce || !snapshot.state.is_live())
                    || (active.is_none() && snapshot.state.is_live())
                {
                    row.runtime = RuntimeState::Conflict;
                }
            } else {
                row.facts.capabilities = crate::agent_workspace::WorkspaceCapabilities::api();
                row.runtime = if active.is_some() {
                    RuntimeState::Running
                } else {
                    RuntimeState::Idle
                };
            }
        }
        Ok(())
    }

    pub(super) fn search(
        &self,
        request: &WorkspaceSearchRequest,
    ) -> Result<WorkspaceSearchPage, String> {
        self.search_with(request, |_| {})
    }

    pub(in crate::agent_workspace) fn search_with(
        &self,
        request: &WorkspaceSearchRequest,
        mut project: impl FnMut(&mut WorkspaceRow),
    ) -> Result<WorkspaceSearchPage, String> {
        let query = WorkspaceQuery::parse(&request.query)?;
        self.advance_search_index()?;
        let limit = request.limit.clamp(1, WORKSPACE_SEARCH_LIMIT);
        let cursor = request.before.map(to_i64).transpose()?.unwrap_or(i64::MAX);
        // Any long positive term is a safe superset; metadata matches must
        // include all documents of that conversation. Short/CJK terms below
        // three characters use the bounded residual scan, never zero results
        // merely because trigram tokenization cannot represent them.
        let phrases: Vec<_> = query
            .plain_terms()
            .into_iter()
            .filter(|term| term.chars().count() >= 3)
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect();
        let fts = (!phrases.is_empty()).then(|| phrases.join(" OR "));
        let mut sql = String::from("SELECT d.document_id,d.event_id,d.revision,d.source,d.message_sequence,d.body,d.truncated,c.row_json FROM workspace_text d JOIN conversations c ON c.id=d.conversation_id WHERE d.document_id<?1 AND (?2 IS NULL OR d.conversation_id=?2)");
        if fts.is_some() {
            sql.push_str(" AND (d.document_id IN (SELECT rowid FROM workspace_text_fts WHERE workspace_text_fts MATCH ?4) OR d.conversation_id IN (SELECT m.conversation_id FROM workspace_text m WHERE m.event_id='metadata' AND m.document_id IN (SELECT rowid FROM workspace_text_fts WHERE workspace_text_fts MATCH ?4)))");
        }
        sql.push_str(" ORDER BY d.document_id DESC LIMIT ?3");
        let mut statement = self.db.prepare(&sql).map_err(err)?;
        statement.raw_bind_parameter(1, cursor).map_err(err)?;
        statement
            .raw_bind_parameter(2, request.conversation.as_ref().map(|id| id.0.as_str()))
            .map_err(err)?;
        statement
            .raw_bind_parameter(3, SEARCH_SCAN_LIMIT as i64 + 1)
            .map_err(err)?;
        if let Some(fts) = fts {
            statement.raw_bind_parameter(4, fts).map_err(err)?;
        }
        let mut values = statement.raw_query();
        let mut hits = Vec::new();
        let mut last_scanned = None;
        let mut next_before = None;
        let mut scanned = 0;
        while let Some(value) = values.next().map_err(err)? {
            if scanned >= SEARCH_SCAN_LIMIT || hits.len() >= limit {
                next_before = last_scanned;
                break;
            }
            let document_id: i64 = value.get(0).map_err(err)?;
            last_scanned = Some(document_id as u64);
            scanned += 1;
            let mut row: WorkspaceRow =
                serde_json::from_str(&value.get::<_, String>(7).map_err(err)?).map_err(err)?;
            self.hydrate_row(&mut row)?;
            project(&mut row);
            if row.archived && !request.include_archived && !query.includes_archived() {
                continue;
            }
            let body: String = value.get(5).map_err(err)?;
            if !query.matches_document(&row, &body) {
                continue;
            }
            let sequence: Option<i64> = value.get(4).map_err(err)?;
            hits.push(WorkspaceSearchHit {
                row,
                anchor: WorkspaceSearchAnchor {
                    event_id: value.get(1).map_err(err)?,
                    revision: value.get(2).map_err(err)?,
                    message_sequence: sequence.map(|n| n as u64),
                },
                source: serde_json::from_str(&value.get::<_, String>(3).map_err(err)?)
                    .map_err(err)?,
                snippet: crate::agent_workspace::search::snippet(&body, &request.query),
                truncated: value.get(6).map_err(err)?,
            });
        }
        let count = |sql| {
            self.db
                .query_row(sql, [], |r| r.get::<_, i64>(0))
                .map(|n| n as usize)
                .map_err(err)
        };
        Ok(WorkspaceSearchPage { hits, next_before, coverage: SearchCoverage {
            conversations: count("SELECT count(*) FROM conversations")?,
            indexed_documents: count("SELECT count(*) FROM workspace_text")?,
            pending_conversations: count("SELECT count(*) FROM workspace_index_pending")?,
            pending_messages: count("SELECT count(*) FROM workspace_message_pending")?,
            external_documents: count("SELECT count(*) FROM workspace_text WHERE source IN ('\"ProviderHistory\"','\"ImportedHistory\"')")?,
            provider_history_complete: false,
        } })
    }

    pub(super) fn read_search_document(
        &self,
        conversation: &ConversationId,
        event_id: &str,
        revision: &str,
    ) -> Result<WorkspaceTextDocument, String> {
        check_text(event_id, 512, "search event identity")?;
        check_text(revision, 256, "search revision")?;
        let record: Option<(String, Option<i64>, String, bool)> = self.db.query_row(
            "SELECT source,message_sequence,body,truncated FROM workspace_text WHERE conversation_id=?1 AND event_id=?2 AND revision=?3",
            params![conversation.0, event_id, revision], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional().map_err(err)?;
        let (source, sequence, text, truncated) = record.ok_or("This indexed revision changed or was deleted. Refresh search to inspect the current evidence.")?;
        Ok(WorkspaceTextDocument {
            conversation: conversation.clone(),
            event_id: event_id.into(),
            revision: revision.into(),
            source: serde_json::from_str(&source).map_err(err)?,
            message_sequence: sequence.map(|n| n as u64),
            text,
            truncated,
        })
    }

    pub(super) fn index_external_text(
        &self,
        documents: Vec<WorkspaceTextDocument>,
    ) -> Result<(), String> {
        if documents.len() > WORKSPACE_TEXT_BATCH {
            return Err("Search import exceeds its 32-record batch limit".into());
        }
        for document in &documents {
            if !matches!(
                document.source,
                SearchSource::ImportedHistory | SearchSource::ProviderHistory
            ) || !document.event_id.starts_with("external:")
                || document.message_sequence.is_some()
                || document.text.len() > WORKSPACE_TEXT_BYTES
                || self.row(&document.conversation)?.is_none()
            {
                return Err("History imports need an existing conversation, an external: event identity, bounded text and read-only provenance".into());
            }
        }
        self.db.execute_batch("BEGIN IMMEDIATE").map_err(err)?;
        let result = documents
            .iter()
            .try_for_each(|document| put_document(&self.db, document));
        if result.is_ok() {
            self.db.execute_batch("COMMIT").map_err(err)?;
        } else {
            let _ = self.db.execute_batch("ROLLBACK");
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Store, Paths) {
        let paths =
            Paths::at(std::env::temp_dir().join(format!("jterm-search-{}", uuid::Uuid::new_v4())))
                .unwrap();
        (Store::open(&paths).unwrap(), paths)
    }
    fn chat(store: &mut Store, title: &str) -> ConversationId {
        let WorkspaceReply::Chat { chat } = store
            .command(WorkspaceCommand::CreateChat {
                title: title.into(),
            })
            .unwrap()
        else {
            panic!("chat")
        };
        chat.row.id
    }
    fn document(
        id: &ConversationId,
        event: &str,
        revision: &str,
        text: &str,
    ) -> WorkspaceTextDocument {
        WorkspaceTextDocument {
            conversation: id.clone(),
            event_id: format!("external:{event}"),
            revision: revision.into(),
            source: SearchSource::ImportedHistory,
            message_sequence: None,
            text: text.into(),
            truncated: false,
        }
    }
    #[test]
    fn unicode_search_revision_and_delete_are_exact_and_read_only() {
        let (mut store, paths) = fixture();
        let id = chat(&mut store, "索引项目");
        store
            .index_external_text(vec![
                document(&id, "first", "1", "支持中文搜索和 café 结果"),
                document(&id, "second", "1", "Another unrelated document"),
            ])
            .unwrap();
        let page = store
            .search(&WorkspaceSearchRequest {
                query: "中文".into(),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.hits.len(), 1);
        let anchor = &page.hits[0].anchor;
        assert!(store
            .read_search_document(&id, &anchor.event_id, &anchor.revision)
            .unwrap()
            .text
            .contains("中文"));
        assert!(store
            .index_external_text(vec![document(
                &id,
                "first",
                "1",
                "changed under the same revision"
            )])
            .is_err());
        assert!(store
            .read_search_document(&id, &anchor.event_id, &anchor.revision)
            .unwrap()
            .text
            .contains("中文"));
        store
            .index_external_text(vec![document(&id, "first", "2", "replacement evidence")])
            .unwrap();
        assert!(store
            .read_search_document(&id, &anchor.event_id, &anchor.revision)
            .is_err());
        assert!(store
            .search(&WorkspaceSearchRequest {
                query: "中文".into(),
                limit: 10,
                ..Default::default()
            })
            .unwrap()
            .hits
            .is_empty());
        store
            .command(WorkspaceCommand::Delete { id: id.clone() })
            .unwrap();
        assert!(store
            .search(&WorkspaceSearchRequest {
                query: "replacement".into(),
                limit: 10,
                ..Default::default()
            })
            .unwrap()
            .hits
            .is_empty());
        drop(store);
        let _ = std::fs::remove_dir_all(paths.state);
    }
    #[test]
    fn fts_keeps_metadata_matches_and_cursor_excludes_new_rows() {
        let (mut store, paths) = fixture();
        let id = chat(&mut store, "orchard project");
        store
            .index_external_text(vec![
                document(&id, "a", "1", "first needle"),
                document(&id, "b", "1", "second needle"),
            ])
            .unwrap();
        let page = store
            .search(&WorkspaceSearchRequest {
                query: "orchard needle".into(),
                limit: 1,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.hits.len(), 1);
        assert!(page.next_before.is_some());
        store
            .index_external_text(vec![document(&id, "c", "1", "new needle")])
            .unwrap();
        let next = store
            .search(&WorkspaceSearchRequest {
                query: "orchard needle".into(),
                before: page.next_before,
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(next.hits.len(), 1);
        assert_ne!(next.hits[0].anchor.event_id, page.hits[0].anchor.event_id);
        assert_ne!(next.hits[0].anchor.event_id, "external:c");
        drop(store);
        let _ = std::fs::remove_dir_all(paths.state);
    }
    #[test]
    fn pagination_has_no_legacy_catalog_cap_and_forged_sources_fail() {
        let (mut store, paths) = fixture();
        let ids: Vec<_> = (0..3)
            .map(|n| chat(&mut store, &format!("Chat {n}")))
            .collect();
        let WorkspaceReply::CatalogPage {
            rows,
            next_before,
            total,
        } = store.catalog_page(None, 2).unwrap()
        else {
            panic!("page")
        };
        assert_eq!(total, 3);
        assert_eq!(rows.len(), 2);
        let WorkspaceReply::CatalogPage {
            rows: older,
            next_before,
            ..
        } = store.catalog_page(next_before, 2).unwrap()
        else {
            panic!("page")
        };
        assert_eq!(older.len(), 1);
        assert!(next_before.is_none());
        assert_eq!(older[0].id, ids[0]);
        let mut forged = document(&ids[0], "bad", "1", "not actual validation");
        forged.source = SearchSource::Validation;
        assert!(store.index_external_text(vec![forged]).is_err());
        drop(store);
        let _ = std::fs::remove_dir_all(paths.state);
    }
    #[test]
    fn ten_thousand_conversations_page_without_gaps_and_cold_index_reports_coverage() {
        let (store, paths) = fixture();
        store.db.execute_batch("BEGIN IMMEDIATE").unwrap();
        for n in 0..10_000 {
            let mut row = super::super::api_row(
                ConversationId(format!("jterm:api:scale-{n}")),
                &format!("Scale fixture {n}"),
            );
            if n == 0 {
                row.title = "Oldest unique evidence".into();
            }
            store.upsert_runtime_row(&row).unwrap();
        }
        store.db.execute_batch("COMMIT").unwrap();
        let mut before = None;
        let mut identities = std::collections::HashSet::new();
        loop {
            let WorkspaceReply::CatalogPage {
                rows,
                next_before,
                total,
            } = store.catalog_page(before, 256).unwrap()
            else {
                panic!("page")
            };
            assert_eq!(total, 10_000);
            for row in rows {
                assert!(identities.insert(row.id));
            }
            before = next_before;
            if before.is_none() {
                break;
            }
        }
        assert_eq!(identities.len(), 10_000);
        let request = WorkspaceSearchRequest {
            query: "\"Oldest unique evidence\"".into(),
            limit: 10,
            ..Default::default()
        };
        let cold = store.search(&request).unwrap();
        assert!(cold.coverage.pending_conversations > 0);
        let mut page = cold;
        for _ in 0..160 {
            if page.coverage.pending_conversations == 0 {
                break;
            }
            page = store.search(&request).unwrap();
        }
        assert_eq!(page.coverage.pending_conversations, 0);
        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.hits[0].row.id.0, "jterm:api:scale-0");
        drop(store);
        let _ = std::fs::remove_dir_all(paths.state);
    }
}
