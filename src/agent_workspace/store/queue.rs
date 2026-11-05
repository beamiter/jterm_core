//! Queue metadata uses the existing host's single SQLite writer. Queue writes
//! never call a provider, mutate a worktree, acquire execution ownership or
//! create a task; they only record the user's dependency intent.
use super::*;
use crate::agent_workspace::{
    WorkspaceQueueChange, WorkspaceQueueSnapshot, WorkspaceQueueUpdate, WorkspaceQueuedIntent,
};

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        "BEGIN;
        CREATE TABLE IF NOT EXISTS workspace_queue_meta (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), revision INTEGER NOT NULL);
        INSERT OR IGNORE INTO workspace_queue_meta VALUES(1,0);
        CREATE TABLE IF NOT EXISTS workspace_queue (
            id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
            intent TEXT NOT NULL, queued_at_ms INTEGER NOT NULL);
        CREATE TRIGGER IF NOT EXISTS workspace_queue_deleted AFTER DELETE ON workspace_queue BEGIN
            UPDATE workspace_queue_meta SET revision=revision+1 WHERE singleton=1; END;
        COMMIT;",
    )
    .map_err(err)
}

impl Store {
    fn queue_value(&self) -> Result<WorkspaceQueueSnapshot, String> {
        let revision: i64 = self
            .db
            .query_row(
                "SELECT revision FROM workspace_queue_meta WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(err)?;
        let mut statement = self
            .db
            .prepare("SELECT intent,queued_at_ms FROM workspace_queue ORDER BY queued_at_ms,id")
            .map_err(err)?;
        let mapped = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(err)?;
        let mut entries = Vec::new();
        for item in mapped {
            let (json, queued_at) = item.map_err(err)?;
            let intent: crate::agent_workspace::WorkspaceQueueIntent =
                serde_json::from_str(&json).map_err(err)?;
            intent.validate()?;
            entries.push(WorkspaceQueuedIntent {
                intent,
                queued_at_ms: u64::try_from(queued_at).map_err(err)?,
            });
        }
        if entries.len() > crate::agent_workspace::WORKSPACE_QUEUE_LIMIT {
            return Err("Stored queue exceeds its supported capacity".into());
        }
        Ok(WorkspaceQueueSnapshot {
            revision: u64::try_from(revision).map_err(err)?,
            entries,
        })
    }

    pub(in crate::agent_workspace) fn queue_snapshot(&self) -> Result<WorkspaceReply, String> {
        Ok(WorkspaceReply::Queue {
            snapshot: self.queue_value()?,
        })
    }

    pub(in crate::agent_workspace) fn queue_update(
        &mut self,
        update: WorkspaceQueueUpdate,
    ) -> Result<WorkspaceReply, String> {
        self.db.execute_batch("BEGIN IMMEDIATE").map_err(err)?;
        let result = (|| {
            let mut snapshot = self.queue_value()?;
            if snapshot.revision != update.expected_revision {
                return Err(
                    "The task queue changed in another view; refresh before retrying".into(),
                );
            }
            if let WorkspaceQueueChange::Upsert { intent } = &update.change {
                intent.validate()?;
                for id in std::iter::once(&intent.id).chain(&intent.dependencies) {
                    let row = self
                        .row(id)?
                        .ok_or("The exact task is no longer in the workspace catalog")?;
                    if row.kind != ConversationKind::Task {
                        return Err("Only engineering tasks can enter the dependency queue".into());
                    }
                }
            }
            snapshot.apply_change(&update.change, now_ms())?;
            match &update.change {
                WorkspaceQueueChange::Upsert { intent } => {
                    let queued_at_ms = snapshot
                        .entries
                        .iter()
                        .find(|entry| entry.intent.id == intent.id)
                        .ok_or("Queue entry disappeared during validation")?
                        .queued_at_ms;
                    self.db
                        .execute(
                            "INSERT INTO workspace_queue(id,intent,queued_at_ms) VALUES(?1,?2,?3)
                        ON CONFLICT(id) DO UPDATE SET intent=excluded.intent",
                            params![
                                intent.id.0,
                                serde_json::to_string(intent).map_err(err)?,
                                to_i64(queued_at_ms)?
                            ],
                        )
                        .map_err(err)?;
                }
                WorkspaceQueueChange::Remove { id } => {
                    self.db
                        .execute("DELETE FROM workspace_queue WHERE id=?1", [&id.0])
                        .map_err(err)?;
                }
            }
            snapshot.revision = snapshot
                .revision
                .checked_add(1)
                .ok_or("Queue revision exhausted")?;
            self.db
                .execute(
                    "UPDATE workspace_queue_meta SET revision=?1 WHERE singleton=1",
                    [to_i64(snapshot.revision)?],
                )
                .map_err(err)?;
            Ok(WorkspaceReply::Queue { snapshot })
        })();
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
    use crate::agent_workspace::{
        WorkspaceExecutionLocation, WorkspaceFacts, WorkspaceQueueIntent,
    };

    fn fixture() -> (Store, Paths) {
        let paths = Paths::at(
            std::env::temp_dir().join(format!("jterm-queue-test-{}", uuid::Uuid::new_v4())),
        )
        .unwrap();
        (Store::open(&paths).unwrap(), paths)
    }
    fn task(store: &Store, name: &str) -> ConversationId {
        let id = crate::agent_workspace::task_id("queue-fixture", name).unwrap();
        store
            .upsert_runtime_row(&WorkspaceRow {
                id: id.clone(),
                kind: ConversationKind::Task,
                title: name.into(),
                provider: "Codex".into(),
                cwd: "/fixture/repo".into(),
                preview: String::new(),
                updated_at_ms: 1,
                archived: false,
                pinned: false,
                tags: Vec::new(),
                runtime: RuntimeState::Idle,
                facts: WorkspaceFacts::default(),
            })
            .unwrap();
        id
    }
    fn update(
        revision: u64,
        id: &ConversationId,
        dependencies: Vec<ConversationId>,
    ) -> WorkspaceQueueUpdate {
        WorkspaceQueueUpdate {
            expected_revision: revision,
            change: WorkspaceQueueChange::Upsert {
                intent: WorkspaceQueueIntent {
                    id: id.clone(),
                    dependencies,
                    location: WorkspaceExecutionLocation::Local,
                },
            },
        }
    }
    fn snapshot(store: &Store) -> WorkspaceQueueSnapshot {
        let WorkspaceReply::Queue { snapshot } = store.queue_snapshot().unwrap() else {
            panic!("queue")
        };
        snapshot
    }
    #[test]
    fn queue_survives_reopen_rejects_stale_writes_and_never_creates_runtime_jobs() {
        if crate::agent_workspace::isolated_lease_test(
            "agent_workspace::store::queue::tests::queue_survives_reopen_rejects_stale_writes_and_never_creates_runtime_jobs",
        ) {
            return;
        }
        let (mut store, paths) = fixture();
        let a = task(&store, "a");
        let b = task(&store, "b");
        store.queue_update(update(0, &a, vec![b.clone()])).unwrap();
        assert!(store.queue_update(update(0, &b, Vec::new())).is_err());
        assert!(store.queue_update(update(1, &b, vec![a.clone()])).is_err());
        let expected = snapshot(&store);
        assert_eq!(expected.revision, 1);
        assert_eq!(expected.entries.len(), 1);
        assert_eq!(
            store
                .db
                .query_row("SELECT COUNT(*) FROM native_jobs", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(store);
        let mut store = Store::open(&paths).unwrap();
        assert_eq!(snapshot(&store), expected);
        store
            .queue_update(WorkspaceQueueUpdate {
                expected_revision: 1,
                change: WorkspaceQueueChange::Remove { id: a },
            })
            .unwrap();
        assert!(snapshot(&store).entries.is_empty());
        assert_eq!(snapshot(&store).revision, 2);
        store.queue_update(update(2, &b, Vec::new())).unwrap();
        store
            .db
            .execute("DELETE FROM conversations WHERE id=?1", [&b.0])
            .unwrap();
        assert!(snapshot(&store).entries.is_empty());
        assert_eq!(
            snapshot(&store).revision,
            4,
            "catalog deletion invalidates stale queue revisions"
        );
        assert!(store.queue_update(update(3, &b, Vec::new())).is_err());
        drop(store);
        let _ = std::fs::remove_dir_all(paths.state);
    }
}
