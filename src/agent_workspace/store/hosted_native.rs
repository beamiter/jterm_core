use super::*;
use crate::agent_workspace::native::{NativeCatalogEntry, NativeSnapshot};
impl Store {
    pub(in crate::agent_workspace) fn save_native(
        &self,
        snapshot: &NativeSnapshot,
    ) -> Result<(), String> {
        let projection = crate::agent_workspace::native::safe_snapshot_projection(snapshot);
        let snapshot = &projection;
        let json = serde_json::to_string(snapshot).map_err(err)?;
        if json.len() > 4 * 1024 * 1024 {
            return Err("Native snapshot exceeds its durable view budget".into());
        }
        self.db.execute_batch("BEGIN IMMEDIATE").map_err(err)?;
        let result = (|| {
            self.upsert_runtime_row(&snapshot.row())?;
            // Task lifecycle archive/restore is a host reducer transition, not
            // an independent catalog decoration. Preserve its pins and title.
            if let Some(mut row) = self.row(&snapshot.id)? {
                row.archived = snapshot.task.status == crate::agent_task::TaskStatus::Archived;
                self.db
                    .execute(
                        "UPDATE conversations SET row_json=?1 WHERE id=?2",
                        params![serde_json::to_string(&row).map_err(err)?, snapshot.id.0],
                    )
                    .map_err(err)?;
            }
            self.db.execute("INSERT INTO native_jobs(id,task_id,incarnation,sequence,snapshot) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET incarnation=excluded.incarnation,sequence=excluded.sequence,snapshot=excluded.snapshot",params![snapshot.id.0,snapshot.task.id.to_string(),snapshot.incarnation,to_i64(snapshot.sequence)?,json]).map_err(err)?;
            if !snapshot.live && !snapshot.preparing && !snapshot.handoff_pending {
                self.index_native_text(snapshot)?;
            }
            Ok(())
        })();
        if result.is_ok() {
            self.db.execute_batch("COMMIT").map_err(err)?;
        } else {
            let _ = self.db.execute_batch("ROLLBACK");
        }
        result
    }
    pub(in crate::agent_workspace) fn reserve_native_operation(
        &self,
        id: &ConversationId,
        incarnation: &str,
        operation: &str,
    ) -> Result<bool, String> {
        self.db.execute("INSERT OR IGNORE INTO native_operations(conversation_id,incarnation,operation) VALUES(?1,?2,?3)",params![id.0,incarnation,operation]).map(|count|count==1).map_err(err)
    }
    pub(in crate::agent_workspace) fn native_snapshot(
        &self,
        id: &ConversationId,
    ) -> Result<Option<NativeSnapshot>, String> {
        let json: Option<String> = self
            .db
            .query_row(
                "SELECT snapshot FROM native_jobs WHERE id=?1",
                [&id.0],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?;
        json.map(|json| serde_json::from_str(&json).map_err(err))
            .transpose()
    }
    pub(in crate::agent_workspace) fn native_catalog(
        &self,
        after: Option<&ConversationId>,
        limit: usize,
    ) -> Result<WorkspaceReply, String> {
        let limit = limit.clamp(1, 128);
        let mut statement = self.db.prepare("SELECT id,task_id,incarnation,sequence FROM native_jobs WHERE id>?1 ORDER BY id LIMIT ?2").map_err(err)?;
        let rows = statement
            .query_map(
                params![
                    after.map(|id| id.0.as_str()).unwrap_or(""),
                    limit as i64 + 1
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .map_err(err)?;
        let mut entries = Vec::new();
        for row in rows {
            let (id, task_id, incarnation, sequence) = row.map_err(err)?;
            entries.push(NativeCatalogEntry {
                id: ConversationId(id),
                task_id: serde_json::from_value(serde_json::Value::String(task_id)).map_err(err)?,
                incarnation,
                sequence: u64::try_from(sequence).map_err(err)?,
            });
        }
        let next_after = if entries.len() > limit {
            entries.truncate(limit);
            entries.last().map(|entry| entry.id.clone())
        } else {
            None
        };
        Ok(WorkspaceReply::NativeCatalog {
            entries,
            next_after,
        })
    }
    pub(super) fn recover_native_jobs(&self) -> Result<(), String> {
        let mut statement = self.db.prepare("SELECT id FROM native_jobs").map_err(err)?;
        let ids: Vec<String> = statement
            .query_map([], |r| r.get(0))
            .map_err(err)?
            .collect::<Result<_, _>>()
            .map_err(err)?;
        drop(statement);
        for id in ids {
            let Some(mut snapshot) = self.native_snapshot(&ConversationId(id))? else {
                continue;
            };
            snapshot.incarnation = uuid::Uuid::new_v4().to_string();
            snapshot.sequence = snapshot.sequence.saturating_add(1);
            snapshot.events.clear();
            if let Some(view) = &mut snapshot.view {
                view.pending_approvals.clear();
                view.history_loading = false;
            }
            if snapshot.live
                || snapshot.preparing
                || snapshot.handoff_pending
                || snapshot.ownership_locked
            {
                snapshot.sequence = snapshot.sequence.saturating_add(1);
                snapshot.live = false;
                snapshot.preparing = false;
                snapshot.handoff_pending = false;
                snapshot.terminal = None;
                if let Some(validation) = &mut snapshot.validation {
                    validation.preparing = false;
                    validation.terminal = None;
                    if snapshot.task.validation.status
                        == crate::agent_task::TaskValidationStatus::Running
                    {
                        snapshot.task.validation.status =
                            crate::agent_task::TaskValidationStatus::Inconclusive;
                        snapshot.task.validation.exit_code = None;
                        snapshot.task.validation.status_detail = Some(
                            "Host restarted before validation completion; no command was repeated"
                                .into(),
                        );
                    }
                }
                snapshot.interrupted = true;
                snapshot.can_prompt = false;
                snapshot.can_resume = false;
                snapshot.can_continue_in_terminal = false;
                snapshot.events.clear();
                snapshot.task.status = crate::agent_task::TaskStatus::Failed;
                snapshot.task.status_detail=Some("Host restarted; the previous native outcome is unknown. No operation was repeated.".into());
                if let Some(view) = &mut snapshot.view {
                    view.phase = crate::agent_task::CodexAppServerPhase::Failed;
                    view.pending_approvals.clear();
                    view.history_loading = false;
                    view.last_error = snapshot.task.status_detail.clone();
                }
            }
            self.save_native(&snapshot)?;
        }
        Ok(())
    }
}
