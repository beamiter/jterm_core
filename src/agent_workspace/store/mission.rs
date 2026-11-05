//! Delivery plans and observations share the session host's single writer.
//! Plans carry no authority to claim test success or remote integration.
use super::*;
use crate::agent_workspace::{WorkspaceMission, WorkspaceMissionReport};

const MISSION_LIMIT: i64 = 64;
const PLAN_BYTES: usize = 128 * 1024;
const REPORT_BYTES: usize = 512 * 1024;

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        "BEGIN;
        CREATE TABLE IF NOT EXISTS workspace_missions (
            id TEXT PRIMARY KEY, revision INTEGER NOT NULL,
            plan_json TEXT NOT NULL, report_json TEXT);
        UPDATE schema_version SET version=6;
        COMMIT;",
    )
    .map_err(err)
}

impl Store {
    pub(in crate::agent_workspace) fn mission_list(&self) -> Result<WorkspaceReply, String> {
        let mut statement = self
            .db
            .prepare("SELECT plan_json FROM workspace_missions ORDER BY id LIMIT ?1")
            .map_err(err)?;
        let rows = statement
            .query_map([MISSION_LIMIT + 1], |row| row.get::<_, String>(0))
            .map_err(err)?;
        let mut missions = Vec::new();
        for row in rows {
            missions.push(serde_json::from_str(&row.map_err(err)?).map_err(err)?);
        }
        if missions.len() > MISSION_LIMIT as usize {
            return Err("Mission catalog exceeds its supported capacity".into());
        }
        Ok(WorkspaceReply::Missions { missions })
    }

    pub(in crate::agent_workspace) fn read_mission(
        &self,
        id: &str,
    ) -> Result<WorkspaceMission, String> {
        check_text(id, 128, "Mission identity")?;
        let value: Option<String> = self
            .db
            .query_row(
                "SELECT plan_json FROM workspace_missions WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(err)?;
        serde_json::from_str(&value.ok_or("Mission no longer exists")?).map_err(err)
    }

    pub(in crate::agent_workspace) fn read_mission_reply(
        &self,
        id: &str,
    ) -> Result<WorkspaceReply, String> {
        let mission = self.read_mission(id)?;
        let json: Option<String> = self
            .db
            .query_row(
                "SELECT report_json FROM workspace_missions WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .map_err(err)?;
        let mut report = json
            .map(|value| serde_json::from_str::<WorkspaceMissionReport>(&value).map_err(err))
            .transpose()?;
        if let Some(report) = &mut report {
            report.require_reinspection();
        }
        Ok(WorkspaceReply::Mission { mission, report })
    }

    pub(in crate::agent_workspace) fn save_mission(
        &mut self,
        expected: u64,
        mut mission: WorkspaceMission,
    ) -> Result<WorkspaceReply, String> {
        mission.validate()?;
        if mission.revision != expected {
            return Err("Mission revision changed; reload before saving".into());
        }
        if serde_json::to_vec(&mission).map_err(err)?.len() > PLAN_BYTES {
            return Err("Mission plan exceeds its 128 KiB budget".into());
        }
        // Task/run references must belong to the existing durable catalog.
        // A stale run remains readable evidence, never a new task or approval.
        for repository in &mission.repositories {
            for task in &repository.tasks {
                let snapshot = self
                    .native_snapshot(&task.conversation)?
                    .ok_or("Linked task is not hosted; inspect the task before linking it")?;
                if snapshot.task.id != task.task_id || snapshot.incarnation != task.run_id {
                    return Err("Linked task run changed; inspect and link the current run".into());
                }
            }
        }
        mission.title = crate::redact::redact_secrets(&mission.title);
        mission.goal = crate::redact::redact_secrets(&mission.goal);
        mission.evidence_after_ms = if expected == 0 {
            now_ms()
        } else {
            let previous = self.read_mission(&mission.id)?;
            if previous.goal != mission.goal
                || previous.required_environment != mission.required_environment
                || previous.repositories != mission.repositories
                || previous.preset != mission.preset
            {
                now_ms().max(previous.evidence_after_ms.saturating_add(1))
            } else {
                previous.evidence_after_ms
            }
        };
        mission.revision = expected
            .checked_add(1)
            .ok_or("Mission revision exhausted")?;
        mission.updated_at_ms = now_ms();
        let json = serde_json::to_string(&mission).map_err(err)?;
        let changed = if expected == 0 {
            let count: i64 = self
                .db
                .query_row("SELECT count(*) FROM workspace_missions", [], |row| row.get(0))
                .map_err(err)?;
            if count >= MISSION_LIMIT {
                return Err("The workspace supports up to 64 saved Missions".into());
            }
            self.db.execute(
                "INSERT OR IGNORE INTO workspace_missions(id,revision,plan_json) VALUES(?1,?2,?3)",
                params![mission.id, to_i64(mission.revision)?, json],
            )
        } else {
            self.db.execute(
                "UPDATE workspace_missions SET revision=?1,plan_json=?2,report_json=NULL WHERE id=?3 AND revision=?4",
                params![to_i64(mission.revision)?, json, mission.id, to_i64(expected)?],
            )
        }
        .map_err(err)?;
        if changed != 1 {
            return Err("Mission changed in another view; reload before retrying. Your draft was not applied.".into());
        }
        Ok(WorkspaceReply::Mission {
            mission,
            report: None,
        })
    }

    pub(in crate::agent_workspace) fn record_mission_report(
        &self,
        mission: WorkspaceMission,
        report: WorkspaceMissionReport,
    ) -> Result<WorkspaceReply, String> {
        if report.id != mission.id || report.revision != mission.revision {
            return Err("Inspection belongs to another Mission revision".into());
        }
        let json = serde_json::to_string(&report).map_err(err)?;
        if json.len() > REPORT_BYTES {
            return Err("Mission inspection exceeds its evidence budget".into());
        }
        let changed = self
            .db
            .execute(
                "UPDATE workspace_missions SET report_json=?1 WHERE id=?2 AND revision=?3",
                params![json, mission.id, to_i64(mission.revision)?],
            )
            .map_err(err)?;
        if changed != 1 {
            return Err("Mission changed during inspection; inspect the current revision".into());
        }
        Ok(WorkspaceReply::Mission {
            mission,
            report: Some(report),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mission_cas_survives_reopen_without_creating_runtime_jobs() {
        if crate::agent_workspace::isolated_lease_test(
            "agent_workspace::store::mission::tests::mission_cas_survives_reopen_without_creating_runtime_jobs",
        ) {
            return;
        }
        let paths =
            Paths::at(std::env::temp_dir().join(format!("jterm-mission-{}", uuid::Uuid::new_v4())))
                .unwrap();
        let mut store = Store::open(&paths).unwrap();
        let mission = WorkspaceMission::new("Release", "Review exact candidates");
        let first = store.save_mission(0, mission.clone()).unwrap();
        assert!(store.save_mission(0, mission.clone()).is_err());
        let WorkspaceReply::Mission { mission: saved, .. } = first else {
            panic!("Mission reply");
        };
        assert_eq!(saved.revision, 1);
        let mut stale_report = saved.project(&[]);
        stale_report.revision = 0;
        assert!(store
            .record_mission_report(saved.clone(), stale_report)
            .is_err());
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM native_jobs", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(store);
        let mut store = Store::open(&paths).unwrap();
        assert_eq!(
            store
                .db
                .query_row("SELECT version FROM schema_version", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            6
        );
        assert_eq!(store.read_mission(&saved.id).unwrap().revision, 1);
        assert!(store.save_mission(0, mission).is_err());
        store.save_mission(1, saved).unwrap();
        drop(store);
        let _ = std::fs::remove_dir_all(paths.state);
    }

    #[test]
    fn saved_report_reopen_keeps_history_without_claiming_current_evidence() {
        if crate::agent_workspace::isolated_lease_test(
            "agent_workspace::store::mission::tests::saved_report_reopen_keeps_history_without_claiming_current_evidence",
        ) {
            return;
        }
        let paths =
            Paths::at(std::env::temp_dir().join(format!("jterm-mission-{}", uuid::Uuid::new_v4())))
                .unwrap();
        let mut store = Store::open(&paths).unwrap();
        let mut mission = WorkspaceMission::new("Release", "Review exact candidates");
        mission.repositories.push(
            crate::agent_workspace::WorkspaceMissionRepository::new("example/repo", "/tmp/repo")
                .unwrap(),
        );
        let WorkspaceReply::Mission { mission, .. } = store.save_mission(0, mission).unwrap()
        else {
            panic!("Mission reply");
        };
        let mut historical = mission.project(&[]);
        historical.state = crate::agent_workspace::WorkspaceMissionState::Completed;
        historical.repositories[0].validated = true;
        historical.repositories[0].checks[0].status =
            crate::agent_workspace::WorkspaceMissionCheckStatus::Passed;
        historical.repositories[0].checks[0].receipt_id = Some("saved-receipt".into());
        store
            .record_mission_report(mission.clone(), historical.clone())
            .unwrap();
        drop(store);
        let store = Store::open(&paths).unwrap();
        let WorkspaceReply::Mission {
            report: Some(report),
            ..
        } = store.read_mission_reply(&mission.id).unwrap()
        else {
            panic!("saved Mission report");
        };
        assert_eq!(
            report.state,
            crate::agent_workspace::WorkspaceMissionState::NeedsAttention
        );
        assert!(!report.repositories[0].validated);
        assert_eq!(
            report.repositories[0].checks[0].status,
            crate::agent_workspace::WorkspaceMissionCheckStatus::Stale
        );
        assert_eq!(
            report.repositories[0].checks[0].receipt_id.as_deref(),
            Some("saved-receipt")
        );
        let json: String = store
            .db
            .query_row(
                "SELECT report_json FROM workspace_missions WHERE id=?1",
                [&mission.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<WorkspaceMissionReport>(&json).unwrap(),
            historical,
            "a read must not overwrite the historical observation"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(paths.state);
    }
}
