use super::*;
use crate::agent_workspace::{WorkspaceContext, WorkspaceHandoff};

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS workspace_context (
        conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
        revision INTEGER NOT NULL, context_json TEXT NOT NULL);",
    )
    .map_err(err)
}

impl Store {
    pub(super) fn read_context(&self, id: &ConversationId) -> Result<WorkspaceContext, String> {
        if self.row(id)?.is_none() {
            return Err("Source conversation no longer exists".into());
        }
        let value: Option<String> = self
            .db
            .query_row(
                "SELECT context_json FROM workspace_context WHERE conversation_id=?1",
                [&id.0],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?;
        value
            .map(|value| serde_json::from_str(&value).map_err(err))
            .unwrap_or_else(|| Ok(WorkspaceContext::empty(id.clone())))
    }
    pub(super) fn save_context(
        &self,
        id: &ConversationId,
        expected: u64,
        mut context: WorkspaceContext,
    ) -> Result<WorkspaceContext, String> {
        if context.conversation != *id || context.revision != expected {
            return Err("Context identity or revision changed; reload before saving".into());
        }
        context.validate()?;
        if self.read_context(id)?.revision != expected {
            return Err(
                "Materials changed in another window; reload before saving your local edits".into(),
            );
        }
        // Source references are exact read-only evidence. Saving a copy does
        // not revive historical approvals or add anything to live model input.
        for material in &context.materials {
            if let Some(source) = &material.source {
                self.read_search_document(
                    &source.conversation,
                    &source.event_id,
                    &source.revision,
                )?;
            }
        }
        for text in [
            &mut context.goal,
            &mut context.completed,
            &mut context.remaining,
            &mut context.constraints,
        ] {
            *text = crate::redact::redact_secrets(text);
        }
        for material in &mut context.materials {
            material.text = crate::redact::redact_secrets(&material.text);
            material.label = crate::redact::redact_secrets(&material.label);
        }
        context.revision = expected
            .checked_add(1)
            .ok_or("Context revision is exhausted")?;
        let changed = self.db.execute("INSERT INTO workspace_context(conversation_id,revision,context_json) VALUES(?1,?2,?3)
            ON CONFLICT(conversation_id) DO UPDATE SET revision=excluded.revision,context_json=excluded.context_json WHERE workspace_context.revision=?4",
            params![id.0, to_i64(context.revision)?, serde_json::to_string(&context).map_err(err)?, to_i64(expected)?]).map_err(err)?;
        if changed != 1 {
            return Err("Materials changed while saving; reload before retrying".into());
        }
        Ok(context)
    }
    pub(super) fn build_handoff(
        &self,
        id: &ConversationId,
        expected: u64,
        target: &str,
    ) -> Result<WorkspaceHandoff, String> {
        let context = self.read_context(id)?;
        if context.revision != expected {
            return Err(
                "Materials changed; preview the current packet before creating a new conversation"
                    .into(),
            );
        }
        for material in &context.materials {
            if let Some(source) = &material.source {
                self.read_search_document(
                    &source.conversation,
                    &source.event_id,
                    &source.revision,
                )?;
            }
        }
        let row = self
            .row(id)?
            .ok_or("Source conversation no longer exists")?;
        context.handoff(target, &row.title, &row.cwd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn materials_are_revision_checked_and_do_not_submit_or_change_history() {
        let paths =
            Paths::at(std::env::temp_dir().join(format!("jterm-context-{}", uuid::Uuid::new_v4())))
                .unwrap();
        let mut store = Store::open(&paths).unwrap();
        let WorkspaceReply::Chat { chat } = store
            .command(WorkspaceCommand::CreateChat {
                title: "Source".into(),
            })
            .unwrap()
        else {
            panic!()
        };
        let mut context = store.read_context(&chat.row.id).unwrap();
        context.goal = "Continue the review".into();
        let saved = store
            .save_context(&chat.row.id, 0, context.clone())
            .unwrap();
        assert_eq!(saved.revision, 1);
        assert!(store.save_context(&chat.row.id, 0, context).is_err());
        assert!(store.build_handoff(&chat.row.id, 0, "Claude").is_err());
        assert_eq!(
            store
                .build_handoff(&chat.row.id, 1, "Claude")
                .unwrap()
                .source,
            chat.row.id
        );
        let count: i64 = store
            .db
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(store.rows().unwrap().len(), 1);
        drop(store);
        let _ = std::fs::remove_dir_all(paths.state);
    }
}
