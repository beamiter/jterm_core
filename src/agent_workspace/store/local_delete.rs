//! Exact local-owned deletion. Provider source files are never touched.
use super::*;

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        "BEGIN IMMEDIATE;
        CREATE TABLE IF NOT EXISTS owned_chat_origins (
            id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
            numeric_id INTEGER NOT NULL UNIQUE CHECK(numeric_id>0));
        COMMIT;",
    )
    .map_err(err)
}

impl Store {
    pub(super) fn insert_local_chat(&mut self, row: &WorkspaceRow) -> Result<(), String> {
        let row_json = serde_json::to_string(row).map_err(err)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(err)?;
        // Insert rather than upsert: a preexisting identity can never
        // acquire local-create provenance, even on an ID collision.
        let inserted = tx
            .execute(
                "INSERT INTO conversations(id,row_json) VALUES(?1,?2)",
                params![row.id.0, row_json],
            )
            .map_err(err)?;
        if inserted != 1 {
            return Err("Local conversation creation did not insert exactly one row".into());
        }
        let registered = tx.execute("INSERT INTO owned_chat_origins(id,numeric_id) SELECT id,numeric_id FROM conversations WHERE id=?1", [&row.id.0]).map_err(err)?;
        if registered != 1 {
            return Err("Local conversation origin was not recorded".into());
        }
        tx.commit().map_err(err)?;
        Ok(())
    }

    fn require_local_delete_origin(&self, id: &ConversationId) -> Result<(), String> {
        // Cheap syntax and provenance gate before any lease file allocation.
        // This read is not authoritative: both operations recheck under lock.
        let candidate = id.0.strip_prefix("jterm:api:").filter(|id| id.len() == 36);
        if candidate
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .is_none()
        {
            return Err("Only locally created workbench conversations can be deleted here".into());
        }
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversations c JOIN owned_chat_origins o ON o.id=c.id AND o.numeric_id=c.numeric_id WHERE c.id=?1)",
            [&id.0], |row| row.get(0),
        ).map_err(err)?;
        if !exists {
            return Err("Conversation is not a locally created workbench conversation".into());
        }
        Ok(())
    }

    pub(super) fn prepare_delete_local_conversation(
        &mut self,
        id: &ConversationId,
    ) -> Result<WorkspaceReply, String> {
        // Keep the lifecycle lock until the read/delete transaction completes.
        self.require_local_delete_origin(id)?;
        let _lifecycle = Lease::acquire(&self.paths.lease(id))?;
        let tx = self.db.transaction().map_err(err)?;
        let snapshot = tx.query_row(
            "SELECT c.numeric_id,c.revision,c.draft_revision,c.row_json,length(CAST(c.draft AS BLOB)),(SELECT count(*) FROM messages WHERE conversation_id=c.id)
             FROM conversations c JOIN owned_chat_origins o ON o.id=c.id AND o.numeric_id=c.numeric_id
             WHERE c.id=?1 AND c.active_nonce IS NULL
             AND NOT EXISTS(SELECT 1 FROM legacy_imports WHERE conversation_id=c.id)
             AND NOT EXISTS(SELECT 1 FROM native_jobs WHERE id=c.id)
             AND NOT EXISTS(SELECT 1 FROM workspace_context WHERE conversation_id=c.id)
             AND NOT EXISTS(SELECT 1 FROM workspace_queue WHERE id=c.id)",
            [&id.0], |r| Ok((r.get::<_,u64>(0)?,r.get::<_,u64>(1)?,r.get::<_,u64>(2)?,r.get::<_,String>(3)?,r.get::<_,u64>(4)?,r.get::<_,u64>(5)?)))
            .optional().map_err(err)?.ok_or("Conversation is active, is not locally owned, or has saved materials or a queue entry")?;
        let row: WorkspaceRow = serde_json::from_str(&snapshot.3).map_err(err)?;
        if row.kind != ConversationKind::Api || row.id != *id {
            return Err("Only locally created workbench conversations can be deleted here".into());
        }
        tx.commit().map_err(err)?;
        Ok(WorkspaceReply::LocalConversationDeletion {
            snapshot: super::super::protocol::LocalConversationDeletionSnapshot {
                id: id.clone(),
                numeric_id: snapshot.0,
                revision: snapshot.1,
                draft_revision: snapshot.2,
                title: row.title,
                draft_bytes: snapshot.4,
                message_count: snapshot.5,
            },
        })
    }
    pub(super) fn delete_local_conversation(
        &mut self,
        id: &ConversationId,
        numeric: u64,
        revision: u64,
        draft_revision: u64,
    ) -> Result<WorkspaceReply, String> {
        let numeric = to_i64(numeric)?;
        let revision = to_i64(revision)?;
        let draft_revision = to_i64(draft_revision)?;
        // Kernel ownership is additional protection, not inferred from catalog status.
        // Keep the lifecycle lock until the read/delete transaction completes.
        self.require_local_delete_origin(id)?;
        let _lifecycle = Lease::acquire(&self.paths.lease(id))?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(err)?;
        let row: Option<String> = tx.query_row(
            "SELECT c.row_json FROM conversations c JOIN owned_chat_origins o ON o.id=c.id AND o.numeric_id=c.numeric_id
             WHERE c.id=?1 AND c.numeric_id=?2 AND c.revision=?3 AND c.draft_revision=?4 AND c.active_nonce IS NULL
             AND NOT EXISTS(SELECT 1 FROM legacy_imports WHERE conversation_id=c.id)
             AND NOT EXISTS(SELECT 1 FROM native_jobs WHERE id=c.id)
             AND NOT EXISTS(SELECT 1 FROM workspace_context WHERE conversation_id=c.id)
             AND NOT EXISTS(SELECT 1 FROM workspace_queue WHERE id=c.id)",
            params![id.0,numeric,revision,draft_revision], |r| r.get(0)).optional().map_err(err)?;
        let row: WorkspaceRow = serde_json::from_str(
            &row.ok_or("Conversation changed, is active, is not locally owned, or has saved materials or a queue entry")?,
        )
        .map_err(err)?;
        if row.kind != ConversationKind::Api || row.id != *id {
            return Err("Only locally created workbench conversations can be deleted here".into());
        }
        let changed = tx.execute("DELETE FROM conversations WHERE id=?1 AND numeric_id=?2 AND revision=?3 AND draft_revision=?4 AND active_nonce IS NULL",params![id.0,numeric,revision,draft_revision]).map_err(err)?;
        if changed != 1 {
            return Err("Conversation changed; nothing was deleted".into());
        }
        tx.commit().map_err(err)?;
        Ok(WorkspaceReply::Ack)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn confirmed_delete_removes_durable_chat_copies_after_reopen() {
        let paths = Paths::at(
            std::env::temp_dir().join(format!("jterm-delete-cascade-{}", uuid::Uuid::new_v4())),
        )
        .unwrap();
        let source = paths.state.join("unrelated-provider-fixture.jsonl");
        let contents = b"fixture provider source\n";
        std::fs::write(&source, contents).unwrap();
        let mut store = Store::open(&paths).unwrap();
        let create = |store: &mut Store, title: &str| match store
            .command(WorkspaceCommand::CreateChat {
                title: title.into(),
            })
            .unwrap()
        {
            WorkspaceReply::Chat { chat } => chat,
            _ => panic!("expected chat"),
        };
        let target = create(&mut store, "target local fixture");
        let keep = create(&mut store, "sibling local fixture");
        let target_id = target.row.id;
        let keep_id = keep.row.id;
        let needle = "retainedfixturemarker";
        for id in [&target_id, &keep_id] {
            append_turns(
                &store.db,
                id,
                &[Turn {
                    role: Role::User,
                    text: needle.into(),
                }],
            )
            .unwrap();
            store
                .command(WorkspaceCommand::SetDraft {
                    id: id.clone(),
                    expected_revision: 0,
                    text: "unsent fixture draft".into(),
                })
                .unwrap();
            store
                .command(WorkspaceCommand::SetReadPosition {
                    id: id.clone(),
                    view: "fixture".into(),
                    position: 12.0,
                })
                .unwrap();
        }
        let search_count = |store: &mut Store, id: &ConversationId| match store
            .command(WorkspaceCommand::Search {
                request: super::super::super::WorkspaceSearchRequest {
                    query: needle.into(),
                    conversation: Some(id.clone()),
                    before: None,
                    limit: 20,
                    include_archived: true,
                },
            })
            .unwrap()
        {
            WorkspaceReply::Search { page } => page.hits.len(),
            _ => panic!("expected search page"),
        };
        assert_eq!(search_count(&mut store, &target_id), 1);
        assert_eq!(search_count(&mut store, &keep_id), 1);
        drop(store);

        let mut store = Store::open(&paths).unwrap();
        let prepared = match store
            .command(WorkspaceCommand::PrepareDeleteLocalConversation {
                id: target_id.clone(),
            })
            .unwrap()
        {
            WorkspaceReply::LocalConversationDeletion { snapshot } => snapshot,
            _ => panic!("expected deletion preview"),
        };
        assert_eq!((prepared.message_count, prepared.draft_bytes), (1, 20));
        assert!(matches!(
            store
                .command(WorkspaceCommand::DeleteLocalConversation {
                    id: prepared.id,
                    expected_numeric_id: prepared.numeric_id,
                    expected_revision: prepared.revision,
                    expected_draft_revision: prepared.draft_revision,
                })
                .unwrap(),
            WorkspaceReply::Ack
        ));
        drop(store);

        let mut store = Store::open(&paths).unwrap();
        assert!(store.load(&target_id).is_err());
        for (table, key) in [
            ("conversations", "id"),
            ("messages", "conversation_id"),
            ("workspace_text", "conversation_id"),
            ("workspace_index_pending", "conversation_id"),
            ("read_positions", "conversation_id"),
            ("owned_chat_origins", "id"),
        ] {
            let count: u64 = store
                .db
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE {key}=?1"),
                    [&target_id.0],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "unexpected surviving entry in {table}");
        }
        assert_eq!(search_count(&mut store, &target_id), 0);
        assert_eq!(search_count(&mut store, &keep_id), 1);
        let sibling = store.load(&keep_id).unwrap();
        assert!(sibling.snapshot_json.contains("unsent fixture draft"));
        assert_eq!(sibling.read_position.unwrap().position, 12.0);
        assert_eq!(std::fs::read(source).unwrap(), contents);
        drop(store);
        std::fs::remove_dir_all(paths.state).unwrap();
    }

    #[test]
    fn confirmed_delete_is_exact_and_rejects_stale_active_and_unowned() {
        let paths =
            Paths::at(std::env::temp_dir().join(format!("jterm-delete-{}", uuid::Uuid::new_v4())))
                .unwrap();
        let mut store = Store::open(&paths).unwrap();
        let create = |store: &mut Store| match store
            .command(WorkspaceCommand::CreateChat {
                title: "Owned fixture".into(),
            })
            .unwrap()
        {
            WorkspaceReply::Chat { chat } => chat.row.id,
            _ => panic!("expected chat"),
        };
        let a = create(&mut store);
        let b = create(&mut store);
        let missing = ConversationId(format!("jterm:api:{}", uuid::Uuid::new_v4()));
        assert!(store.prepare_delete_local_conversation(&missing).is_err());
        assert!(store.delete_local_conversation(&missing, 1, 0, 0).is_err());
        assert!(!paths.lease(&missing).exists());
        let imported = api_row(missing.clone(), "older unmarked chat");
        store.upsert_runtime_row(&imported).unwrap();
        assert!(store
            .insert_local_chat(&api_row(missing.clone(), "must not overwrite"))
            .is_err());
        assert_eq!(
            store.row(&missing).unwrap().unwrap().title,
            "older unmarked chat"
        );
        assert!(store.prepare_delete_local_conversation(&missing).is_err());
        assert!(!paths.lease(&missing).exists());

        let prepared = match store
            .command(WorkspaceCommand::PrepareDeleteLocalConversation { id: a.clone() })
            .unwrap()
        {
            WorkspaceReply::LocalConversationDeletion { snapshot } => snapshot,
            _ => panic!("expected scoped eligibility"),
        };
        assert_eq!(prepared.id, a);
        assert!(prepared.numeric_id < SHARED_CHAT_ID_BASE);
        assert_eq!(prepared.message_count, 0);
        // These writes do not advance the conversation revision.
        for (insert, cleanup) in [
            (
                "INSERT INTO workspace_context VALUES(?1,1,'{}')",
                "DELETE FROM workspace_context WHERE conversation_id=?1",
            ),
            (
                "INSERT INTO workspace_queue VALUES(?1,'{}',0)",
                "DELETE FROM workspace_queue WHERE id=?1",
            ),
        ] {
            store.db.execute(insert, [&a.0]).unwrap();
            assert!(store.prepare_delete_local_conversation(&a).is_err());
            assert!(store
                .delete_local_conversation(
                    &a,
                    prepared.numeric_id,
                    prepared.revision,
                    prepared.draft_revision
                )
                .is_err());
            assert!(store.row(&a).unwrap().is_some());
            assert_eq!(store.db.execute(cleanup, [&a.0]).unwrap(), 1);
        }

        assert!(store
            .delete_local_conversation(
                &a,
                prepared.numeric_id + SHARED_CHAT_ID_BASE,
                prepared.revision,
                prepared.draft_revision
            )
            .is_err());
        assert!(store
            .delete_local_conversation(
                &a,
                prepared.numeric_id,
                prepared.revision,
                prepared.draft_revision + 1
            )
            .is_err());
        let owner = Lease::acquire(&paths.lease(&a)).unwrap();
        assert!(store.prepare_delete_local_conversation(&a).is_err());
        assert!(store
            .delete_local_conversation(
                &a,
                prepared.numeric_id,
                prepared.revision,
                prepared.draft_revision
            )
            .is_err());
        drop(owner);

        let numeric: i64 = store
            .db
            .query_row(
                "SELECT numeric_id FROM conversations WHERE id=?1",
                [&a.0],
                |r| r.get(0),
            )
            .unwrap();
        store.db.execute("INSERT INTO messages(conversation_id,role,body) VALUES(?1,'user','fixture history')",[&a.0]).unwrap();
        let revision: i64 = store
            .db
            .query_row(
                "SELECT revision FROM conversations WHERE id=?1",
                [&a.0],
                |r| r.get(0),
            )
            .unwrap();
        assert!(store
            .delete_local_conversation(&a, numeric as u64, revision as u64 + 1, 0)
            .is_err());
        store
            .db
            .execute(
                "UPDATE conversations SET active_nonce='fixture' WHERE id=?1",
                [&a.0],
            )
            .unwrap();
        assert!(store
            .delete_local_conversation(&a, numeric as u64, revision as u64, 0)
            .is_err());
        store
            .db
            .execute(
                "UPDATE conversations SET active_nonce=NULL WHERE id=?1",
                [&a.0],
            )
            .unwrap();
        store
            .delete_local_conversation(&a, numeric as u64, revision as u64, 0)
            .unwrap();
        assert!(store.row(&a).unwrap().is_none());
        assert!(store.row(&b).unwrap().is_some());
        let count: i64 = store
            .db
            .query_row(
                "SELECT count(*) FROM messages WHERE conversation_id=?1",
                [&a.0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        assert!(store
            .delete_local_conversation(&a, numeric as u64, revision as u64, 0)
            .is_err());
        store
            .db
            .execute("DELETE FROM owned_chat_origins WHERE id=?1", [&b.0])
            .unwrap();
        let bn: i64 = store
            .db
            .query_row(
                "SELECT numeric_id FROM conversations WHERE id=?1",
                [&b.0],
                |r| r.get(0),
            )
            .unwrap();
        assert!(store
            .delete_local_conversation(&b, bn as u64, 0, 0)
            .is_err());
    }
}
