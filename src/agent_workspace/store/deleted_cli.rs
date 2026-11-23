use super::{err, Store};
use crate::agent_sessions::AgentSession;
use crate::agent_workspace::{session_id, ConversationId, ConversationKind, WorkspaceReply};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS deleted_cli_sessions (id TEXT PRIMARY KEY);
         CREATE TRIGGER IF NOT EXISTS ignore_deleted_cli_sessions
         BEFORE INSERT ON conversations
         WHEN EXISTS(SELECT 1 FROM deleted_cli_sessions WHERE id = new.id)
         BEGIN SELECT RAISE(IGNORE); END;",
    )
    .map_err(err)
}

impl Store {
    pub(in crate::agent_workspace) fn cli_session_deleted(
        &self,
        id: &ConversationId,
    ) -> Result<bool, String> {
        self.db
            .prepare_cached("SELECT 1 FROM deleted_cli_sessions WHERE id=?1")
            .map_err(err)?
            .exists([&id.0])
            .map_err(err)
    }

    pub(in crate::agent_workspace) fn delete_cli_session(
        &mut self,
        id: &ConversationId,
    ) -> Result<WorkspaceReply, String> {
        if self.cli_session_deleted(id)? {
            return Ok(WorkspaceReply::Ack);
        }
        if !self
            .row(id)?
            .is_some_and(|row| row.kind == ConversationKind::Cli)
        {
            return Err(
                "This CLI session is missing from the workspace index. Refresh and try again."
                    .into(),
            );
        }
        self.ensure_idle(id)?;
        let tx = self.db.transaction().map_err(err)?;
        tx.execute("INSERT INTO deleted_cli_sessions(id) VALUES (?1)", [&id.0])
            .map_err(err)?;
        tx.execute("DELETE FROM conversations WHERE id=?1", [&id.0])
            .map_err(err)?;
        tx.commit().map_err(err)?;
        Ok(WorkspaceReply::Ack)
    }
}

pub(crate) fn filter_deleted_cli_sessions(sessions: &mut Vec<AgentSession>) -> Result<(), String> {
    let paths = super::Paths::locate()?;
    filter_for_database(&paths.state.join("workspace.sqlite3"), sessions)
}

fn filter_for_database(path: &Path, sessions: &mut Vec<AgentSession>) -> Result<(), String> {
    if sessions.is_empty() || !path.is_file() {
        return Ok(());
    }
    // The session host is still the only writer. Discovery uses a consistent
    // read-only snapshot so it can filter history before publishing any rows.
    let mut db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(err)?;
    let tx = db.transaction().map_err(err)?;
    let has_table = tx
        .prepare("SELECT 1 FROM sqlite_schema WHERE name='deleted_cli_sessions' AND type='table'")
        .map_err(err)?
        .exists([])
        .map_err(err)?;
    if !has_table
        || !tx
            .prepare("SELECT 1 FROM deleted_cli_sessions LIMIT 1")
            .map_err(err)?
            .exists([])
            .map_err(err)?
    {
        return Ok(());
    }
    let mut check = tx
        .prepare("SELECT 1 FROM deleted_cli_sessions WHERE id=?1")
        .map_err(err)?;
    let mut visible = Vec::with_capacity(sessions.len());
    for session in sessions.iter() {
        let deleted = match session_id(session) {
            Ok(id) => check.exists([id.0]).map_err(err)?,
            Err(_) => false,
        };
        visible.push(!deleted);
    }
    let mut visible = visible.into_iter();
    sessions.retain(|_| visible.next().unwrap_or(true));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_sessions::{SessionProvider, SessionStatus};
    use crate::agent_workspace::{WorkspaceCommand, WorkspaceRow, WorkspaceSearchRequest};

    #[test]
    fn deleted_cli_history_stays_out_after_reopen_and_source_rescan() {
        if crate::agent_workspace::isolated_lease_test(
            "agent_workspace::store::deleted_cli::tests::deleted_cli_history_stays_out_after_reopen_and_source_rescan",
        ) {
            return;
        }
        let dir = std::env::temp_dir().join(format!("jterm-cli-delete-{}", uuid::Uuid::new_v4()));
        let paths = super::super::Paths::at(dir.clone()).unwrap();
        let fixture = |profile: &str, title: &str| {
            let source = dir.join(profile).join("sessions/history.jsonl");
            std::fs::create_dir_all(source.parent().unwrap()).unwrap();
            std::fs::write(&source, title).unwrap();
            AgentSession {
                provider: SessionProvider::Codex,
                id: "01912345-1234-7123-8123-123456789abc".into(),
                title: title.into(),
                cwd: dir.clone(),
                updated_at_ms: 1,
                status: SessionStatus::Idle,
                preview: String::new(),
                branch: None,
                source,
                archived: false,
            }
        };
        let first = fixture("one", "unique cairn transcript");
        let second = fixture("two", "other provider profile");
        let rows = vec![
            WorkspaceRow::from_agent(&first).unwrap(),
            WorkspaceRow::from_agent(&second).unwrap(),
        ];
        let id = rows[0].id.clone();
        let mut store = Store::open(&paths).unwrap();
        store
            .command(WorkspaceCommand::IndexSessions {
                sessions: rows.clone(),
            })
            .unwrap();
        let request = WorkspaceSearchRequest {
            query: "cairn".into(),
            conversation: None,
            before: None,
            limit: 10,
            include_archived: true,
        };
        let anchor = store.search(&request).unwrap().hits.remove(0).anchor;
        store.delete_cli_session(&id).unwrap();
        store.delete_cli_session(&id).unwrap();
        assert!(store
            .read_search_document(&id, &anchor.event_id, &anchor.revision)
            .is_err());
        assert_eq!(std::fs::read_to_string(&first.source).unwrap(), first.title);
        drop(store);
        let mut store = Store::open(&paths).unwrap();
        store
            .command(WorkspaceCommand::IndexSessions { sessions: rows })
            .unwrap();
        assert!(store.search(&request).unwrap().hits.is_empty());
        assert_eq!(store.rows().unwrap()[0].id, session_id(&second).unwrap());
        let mut discovered = vec![first, second.clone()];
        filter_for_database(&dir.join("workspace.sqlite3"), &mut discovered).unwrap();
        assert_eq!(discovered, vec![second]);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
