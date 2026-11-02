use super::*;
use crate::agent_workspace::api::{ApiSnapshot, ApiState};

impl Store {
    pub(in crate::agent_workspace) fn resolve_unadmitted_api(
        &mut self,
        receipt: &SubmissionReceipt,
        owner: &(u32, String),
    ) -> Result<WorkspaceReply, String> {
        let owned: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1 AND epoch=?2 AND active_nonce=?3 AND submitted_draft_revision=?4 AND owner_pid=?5 AND owner_start=?6)", params![receipt.id.0,to_i64(receipt.epoch)?,receipt.nonce,to_i64(receipt.draft_revision)?,owner.0,owner.1], |r| r.get(0)).map_err(err)?;
        if !owned {
            return Err("Cannot resolve a stale or foreign API submission".into());
        }
        let tx = self.db.transaction().map_err(err)?;
        cancel_receipt(&tx, receipt, true)?;
        tx.commit().map_err(err)?;
        self.load(&receipt.id)
            .map(|chat| WorkspaceReply::Chat { chat })
    }
    pub(in crate::agent_workspace) fn submission_text(
        &self,
        receipt: &SubmissionReceipt,
    ) -> Result<String, String> {
        self.db.query_row("SELECT submitted_text FROM conversations WHERE id=?1 AND epoch=?2 AND active_nonce=?3 AND submitted_draft_revision=?4", params![receipt.id.0,to_i64(receipt.epoch)?,receipt.nonce,to_i64(receipt.draft_revision)?], |r| r.get(0)).map_err(err)
    }
    pub(in crate::agent_workspace) fn assign_api_worker(
        &self,
        receipt: &SubmissionReceipt,
        pid: u32,
        start: &str,
    ) -> Result<(), String> {
        let changed=self.db.execute("UPDATE conversations SET owner_pid=?1,owner_start=?2 WHERE id=?3 AND epoch=?4 AND active_nonce=?5 AND submitted_draft_revision=?6",params![pid,start,receipt.id.0,to_i64(receipt.epoch)?,receipt.nonce,to_i64(receipt.draft_revision)?]).map_err(err)?;
        if changed != 1 {
            return Err("Stale background API worker identity".into());
        }
        Ok(())
    }
    pub(super) fn recover_api_jobs(&self) -> Result<(), String> {
        let mut stmt = self
            .db
            .prepare("SELECT id,snapshot FROM api_jobs")
            .map_err(err)?;
        let jobs: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(err)?
            .collect::<Result<_, _>>()
            .map_err(err)?;
        drop(stmt);
        for (id, json) in jobs {
            let mut snapshot: ApiSnapshot = serde_json::from_str(&json).map_err(err)?;
            if snapshot.state.is_live() {
                snapshot.sequence += 1;
                snapshot.state = ApiState::Interrupted;
                snapshot.error = Some("Session host restarted; the previous request outcome is unknown. It will not be submitted again automatically.".into());
                self.db
                    .execute(
                        "UPDATE api_jobs SET snapshot=?1 WHERE id=?2",
                        params![serde_json::to_string(&snapshot).map_err(err)?, id],
                    )
                    .map_err(err)?;
            }
        }
        Ok(())
    }
    pub(in crate::agent_workspace) fn api_snapshot(
        &self,
        id: &ConversationId,
    ) -> Result<Option<ApiSnapshot>, String> {
        let json: Option<String> = self
            .db
            .query_row("SELECT snapshot FROM api_jobs WHERE id=?1", [&id.0], |r| {
                r.get(0)
            })
            .optional()
            .map_err(err)?;
        json.map(|s| serde_json::from_str(&s).map_err(err))
            .transpose()
    }
    pub(in crate::agent_workspace) fn api_matches(
        &self,
        receipt: &SubmissionReceipt,
    ) -> Result<bool, String> {
        self.db.query_row("SELECT EXISTS(SELECT 1 FROM api_jobs WHERE id=?1 AND epoch=?2 AND nonce=?3 AND draft_revision=?4)",params![receipt.id.0,to_i64(receipt.epoch)?,receipt.nonce,to_i64(receipt.draft_revision)?],|r|r.get(0)).map_err(err)
    }
    pub(in crate::agent_workspace) fn admit_api(
        &mut self,
        receipt: &SubmissionReceipt,
        client: &(u32, String),
        host: &(u32, String),
        snapshot: &ApiSnapshot,
    ) -> Result<(), String> {
        if self.load(&receipt.id)?.row.kind != ConversationKind::Api {
            return Err("Not an API conversation".into());
        }
        let tx = self.db.transaction().map_err(err)?;
        let changed = tx.execute("UPDATE conversations SET owner_pid=?1,owner_start=?2 WHERE id=?3 AND epoch=?4 AND active_nonce=?5 AND submitted_draft_revision=?6 AND owner_pid=?7 AND owner_start=?8",params![host.0,host.1,receipt.id.0,to_i64(receipt.epoch)?,receipt.nonce,to_i64(receipt.draft_revision)?,client.0,client.1]).map_err(err)?;
        if changed != 1 {
            return Err("API launch does not own the acknowledged submission".into());
        }
        tx.execute("INSERT INTO api_jobs(id,epoch,nonce,draft_revision,snapshot) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET epoch=excluded.epoch,nonce=excluded.nonce,draft_revision=excluded.draft_revision,snapshot=excluded.snapshot",params![receipt.id.0,to_i64(receipt.epoch)?,receipt.nonce,to_i64(receipt.draft_revision)?,serde_json::to_string(snapshot).map_err(err)?]).map_err(err)?;
        tx.commit().map_err(err)
    }
    pub(in crate::agent_workspace) fn save_api_snapshot(
        &self,
        receipt: &SubmissionReceipt,
        snapshot: &ApiSnapshot,
    ) -> Result<(), String> {
        let changed=self.db.execute("UPDATE api_jobs SET snapshot=?1 WHERE id=?2 AND epoch=?3 AND nonce=?4 AND draft_revision=?5",params![serde_json::to_string(snapshot).map_err(err)?,receipt.id.0,to_i64(receipt.epoch)?,receipt.nonce,to_i64(receipt.draft_revision)?]).map_err(err)?;
        if changed != 1 {
            return Err("Stale API runtime snapshot".into());
        }
        Ok(())
    }
    pub(in crate::agent_workspace) fn settle_api(
        &mut self,
        receipt: &SubmissionReceipt,
        result: Result<String, String>,
        cancelled: bool,
    ) -> Result<ApiSnapshot, String> {
        if !self.api_matches(receipt)? {
            return Err("Stale API completion".into());
        }
        let mut snapshot = self
            .api_snapshot(&receipt.id)?
            .ok_or("API runtime is unavailable")?;
        snapshot.sequence += 1;
        let tx = self.db.transaction().map_err(err)?;
        match result {
            Ok(answer) if !cancelled => {
                // The complete turn and terminal snapshot commit together.
                complete_receipt(&tx, receipt, &answer)?;
                snapshot.state = ApiState::Completed;
                snapshot.preview = preview(&answer);
                snapshot.result = Some(answer);
                snapshot.error = None;
            }
            other => {
                cancel_receipt(&tx, receipt, true)?;
                snapshot.state = if cancelled {
                    ApiState::Cancelled
                } else {
                    ApiState::Failed
                };
                snapshot.error = Some(match other {
                    Err(e) => e,
                    Ok(_) => "Response cancelled".into(),
                });
            }
        }
        let changed=tx.execute("UPDATE api_jobs SET snapshot=?1 WHERE id=?2 AND epoch=?3 AND nonce=?4 AND draft_revision=?5",params![serde_json::to_string(&snapshot).map_err(err)?,receipt.id.0,to_i64(receipt.epoch)?,receipt.nonce,to_i64(receipt.draft_revision)?]).map_err(err)?;
        if changed != 1 {
            return Err("Stale API completion".into());
        }
        tx.commit().map_err(err)?;
        Ok(snapshot)
    }
}
