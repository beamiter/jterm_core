//! UI-neutral asynchronous delivery-plan controller for the four renderers.
//! Selection uses stable Mission IDs. Failed writes are read back, never replayed.
use super::*;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

enum Pending {
    Catalog,
    Read {
        id: String,
        generation: u64,
    },
    Write {
        mission: WorkspaceMission,
        generation: u64,
    },
    Reconcile {
        mission: WorkspaceMission,
        generation: u64,
    },
    Inspect {
        id: String,
        generation: u64,
    },
    Link {
        id: ConversationId,
        mission: String,
        generation: u64,
    },
}

/// Native UIs own editable drafts; the controller holds the last host snapshot.
/// Calling `poll` does not start a host or an operation until the user opens the
/// Mission view and requests `refresh`, `create`, `select` or `inspect`.
pub struct MissionWorkbench {
    client: WorkspaceClient,
    pending: HashMap<u64, Pending>,
    generation: u64,
    reports: HashMap<String, WorkspaceMissionReport>,
    pub missions: Vec<WorkspaceMission>,
    pub selected: Option<WorkspaceMission>,
    pub report: Option<WorkspaceMissionReport>,
    pub busy: bool,
    pub notice: Option<String>,
}

impl Default for MissionWorkbench {
    fn default() -> Self {
        Self::new()
    }
}

impl MissionWorkbench {
    pub fn new() -> Self {
        Self::with_client(WorkspaceClient::new())
    }

    pub fn with_client(client: WorkspaceClient) -> Self {
        Self {
            client,
            pending: HashMap::new(),
            generation: 0,
            reports: HashMap::new(),
            missions: Vec::new(),
            selected: None,
            report: None,
            busy: false,
            notice: None,
        }
    }

    fn send(&mut self, command: WorkspaceCommand, pending: Pending) {
        match self.client.send(command) {
            Ok(id) => {
                self.pending.insert(id, pending);
                self.busy = true;
            }
            Err(error) => self.notice = Some(error),
        }
    }

    pub fn refresh(&mut self) {
        if !self
            .pending
            .values()
            .any(|value| matches!(value, Pending::Catalog))
        {
            self.send(WorkspaceCommand::MissionList, Pending::Catalog);
        }
    }

    pub fn select(&mut self, id: &str) {
        self.generation = self.generation.wrapping_add(1);
        self.selected = self
            .missions
            .iter()
            .find(|mission| mission.id == id)
            .cloned();
        self.report = None;
        self.notice = None;
        self.send(
            WorkspaceCommand::ReadMission { id: id.into() },
            Pending::Read {
                id: id.into(),
                generation: self.generation,
            },
        );
    }

    pub fn create(&mut self, title: &str, goal: &str, roots: &[(String, PathBuf)]) {
        if self
            .selected
            .as_ref()
            .is_some_and(|mission| mission.revision == 0)
        {
            self.notice = Some("An unsaved Mission is selected. Edit and save that stable identity before creating another.".into());
            return;
        }
        if self.busy {
            self.notice =
                Some("Wait for the current Mission request before creating another".into());
            return;
        }
        let mut mission = WorkspaceMission::new(title, goal);
        for (name, path) in roots {
            match WorkspaceMissionRepository::new(name, path) {
                Ok(repository) => mission.repositories.push(repository),
                Err(error) => {
                    self.notice = Some(error);
                    return;
                }
            }
        }
        let names: HashSet<_> = roots
            .iter()
            .map(|(name, _)| name.to_ascii_lowercase())
            .collect();
        if names.len() == 5
            && ["jterm_core", "anvil", "forge", "ember", "frost"]
                .iter()
                .all(|name| names.contains(&format!("beamiter/{name}")))
        {
            mission.preset = WorkspaceMissionPreset::FourTerminals;
        }
        self.generation = self.generation.wrapping_add(1);
        self.selected = Some(mission.clone());
        self.report = None;
        self.save(mission);
    }

    pub fn save(&mut self, mission: WorkspaceMission) {
        if self
            .pending
            .values()
            .any(|value| matches!(value, Pending::Write { .. } | Pending::Reconcile { .. }))
        {
            self.notice =
                Some("A Mission write is pending; read its outcome before saving again".into());
            return;
        }
        if let Err(error) = mission.validate() {
            self.notice = Some(error);
            return;
        }
        self.notice = None;
        self.send(
            WorkspaceCommand::SaveMission {
                expected_revision: mission.revision,
                mission: mission.clone(),
            },
            Pending::Write {
                mission,
                generation: self.generation,
            },
        );
    }

    pub fn inspect(&mut self) {
        let Some(mission) = &self.selected else {
            return;
        };
        if self.busy {
            return;
        }
        let id = mission.id.clone();
        self.notice = None;
        self.send(
            WorkspaceCommand::InspectMission { id: id.clone() },
            Pending::Inspect {
                id,
                generation: self.generation,
            },
        );
    }

    /// Explicit user action: freeze observed commits, preserving the original
    /// baseline after its first capture. Changing candidates invalidates reports.
    pub fn adopt_candidates(&mut self) {
        let (Some(mut mission), Some(report)) = (self.selected.clone(), self.report.as_ref())
        else {
            return;
        };
        if self.busy || report.id != mission.id || report.revision != mission.revision {
            return;
        }
        for repository in &mut mission.repositories {
            if let Some(observed) = report
                .repositories
                .iter()
                .find(|entry| entry.identity.matches(&repository.identity))
                .and_then(|entry| entry.observed.as_ref())
            {
                if observed.dirty {
                    self.notice = Some(
                        "Commit or review dirty input before freezing a delivery candidate".into(),
                    );
                    return;
                }
                if repository.baseline.is_empty() {
                    repository.baseline = observed.baseline.clone();
                }
                repository.candidate = Some(observed.head.clone());
            } else {
                self.notice =
                    Some("Inspect every repository before freezing the candidate set".into());
                return;
            }
        }
        if mission.preset == WorkspaceMissionPreset::FourTerminals {
            let core = mission
                .repositories
                .iter()
                .find(|repository| {
                    repository
                        .identity
                        .full_name
                        .eq_ignore_ascii_case("beamiter/jterm_core")
                })
                .and_then(|repository| repository.candidate.clone());
            for repository in &mut mission.repositories {
                if !repository
                    .identity
                    .full_name
                    .eq_ignore_ascii_case("beamiter/jterm_core")
                {
                    repository.required_core_pin = core.clone();
                }
            }
        }
        self.save(mission);
    }

    pub fn link_conversation(&mut self, id: &ConversationId) {
        let Some(mission) = &self.selected else {
            return;
        };
        if self.busy {
            return;
        }
        self.send(
            WorkspaceCommand::InspectNative { id: id.clone() },
            Pending::Link {
                id: id.clone(),
                mission: mission.id.clone(),
                generation: self.generation,
            },
        );
    }

    pub fn link_result(&mut self, report: &WorkspaceResultReport) {
        let Some(mut mission) = self.selected.clone() else {
            return;
        };
        let Some(path) = report
            .validation
            .as_ref()
            .map(|evidence| PathBuf::from(&evidence.cwd))
        else {
            self.notice = Some("Inspect the hosted task to link its exact worktree and run".into());
            return;
        };
        let Some(repository) = mission
            .repositories
            .iter_mut()
            .find(|repository| repository.path == path)
        else {
            self.notice = Some("The result belongs to a worktree outside this Mission".into());
            return;
        };
        let task = WorkspaceMissionTaskRef::from_result(report);
        repository
            .tasks
            .retain(|linked| linked.conversation != task.conversation);
        repository.tasks.push(task);
        self.save(mission);
    }

    /// Uninspected plans stay visible as requiring review, never presumed done.
    pub fn needs_attention(&self) -> usize {
        self.missions
            .iter()
            .filter(|mission| {
                self.reports.get(&mission.id).is_none_or(|report| {
                    report.revision != mission.revision
                        || report.state != WorkspaceMissionState::Completed
                })
            })
            .count()
    }

    pub fn poll(&mut self) {
        for WorkspaceEvent::Reply { request_id, result } in self.client.poll() {
            self.receive(request_id, result);
        }
        self.busy = !self.pending.is_empty();
    }

    fn receive(&mut self, request_id: u64, result: Result<WorkspaceReply, String>) {
        let Some(pending) = self.pending.remove(&request_id) else {
            return;
        };
        match pending {
            Pending::Catalog => match result {
                Ok(WorkspaceReply::Missions { missions }) => {
                    if let Some(selected) = &self.selected {
                        if let Some(current) =
                            missions.iter().find(|mission| mission.id == selected.id)
                        {
                            if current.revision != selected.revision {
                                self.selected = Some(current.clone());
                                self.report = None;
                                self.notice = Some("The saved plan changed in another view. Review the current revision; your local draft remains separate.".into());
                            }
                        } else if selected.revision > 0 {
                            self.report = None;
                            self.notice = Some(
                                "The selected Mission is absent from the current catalog".into(),
                            );
                        }
                    }
                    self.reports.retain(|id, report| {
                        missions
                            .iter()
                            .any(|mission| &mission.id == id && mission.revision == report.revision)
                    });
                    self.missions = missions;
                }
                other => self.unexpected(other),
            },
            Pending::Read { id, generation } | Pending::Inspect { id, generation } => {
                if generation != self.generation {
                    return;
                }
                match result {
                    Ok(WorkspaceReply::Mission { mission, report }) if mission.id == id => {
                        self.apply(mission, report)
                    }
                    other => self.unexpected(other),
                }
            }
            Pending::Write {
                mission: attempt,
                generation,
            } => match result {
                Ok(WorkspaceReply::Mission { mission, report }) if mission.id == attempt.id => {
                    if generation == self.generation {
                        self.apply(mission, report);
                    }
                    self.refresh();
                }
                other => {
                    self.unexpected(other);
                    let detail = self.notice.clone().unwrap_or_default();
                    self.notice = Some(format!("{detail} Reading the saved plan to reconcile this write; no write was replayed."));
                    self.send(
                        WorkspaceCommand::ReadMission {
                            id: attempt.id.clone(),
                        },
                        Pending::Reconcile {
                            mission: attempt,
                            generation,
                        },
                    );
                }
            },
            Pending::Reconcile {
                mission: attempt,
                generation,
            } => {
                if generation != self.generation {
                    return;
                }
                match result {
                    Ok(WorkspaceReply::Mission { mission, report }) if mission.id == attempt.id => {
                        self.apply(mission, report);
                        self.notice = Some("Saved revision reloaded. Compare it with your draft before saving again; no write was replayed.".into());
                        self.refresh();
                    }
                    other => self.unexpected(other),
                }
            }
            Pending::Link {
                id,
                mission,
                generation,
            } => {
                if generation != self.generation
                    || self
                        .selected
                        .as_ref()
                        .is_none_or(|selected| selected.id != mission)
                {
                    return;
                }
                match result {
                    Ok(WorkspaceReply::Native { snapshot, .. }) if snapshot.id == id => {
                        let Some(mut mission) = self.selected.clone() else {
                            return;
                        };
                        let Some(repository) = mission
                            .repositories
                            .iter_mut()
                            .find(|repository| repository.path == snapshot.task.worktree_path)
                        else {
                            self.notice = Some("Set this Mission's repository path to the task's exact worktree before linking".into());
                            return;
                        };
                        repository
                            .tasks
                            .retain(|task| task.conversation != snapshot.id);
                        repository.tasks.push(WorkspaceMissionTaskRef {
                            conversation: snapshot.id.clone(),
                            task_id: snapshot.task.id,
                            run_id: snapshot.incarnation.clone(),
                            source_sequence: snapshot.sequence,
                        });
                        if repository.baseline.is_empty() {
                            repository.baseline = snapshot.task.base_commit.clone();
                        }
                        self.save(mission);
                    }
                    other => self.unexpected(other),
                }
            }
        }
    }

    fn apply(&mut self, mission: WorkspaceMission, report: Option<WorkspaceMissionReport>) {
        if let Some(report) = &report {
            self.reports.insert(mission.id.clone(), report.clone());
        } else {
            self.reports.remove(&mission.id);
        }
        if let Some(existing) = self
            .missions
            .iter_mut()
            .find(|existing| existing.id == mission.id)
        {
            *existing = mission.clone();
        } else {
            self.missions.push(mission.clone());
        }
        self.selected = Some(mission);
        self.report = report;
    }

    fn unexpected(&mut self, reply: Result<WorkspaceReply, String>) {
        self.notice = Some(match reply {
            Err(error) => format!("Mission request: {error}. If the host predates Missions, finish active work and restart it with the updated frontend."),
            Ok(_) => "The host returned an unexpected Mission reply".into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workbench() -> MissionWorkbench {
        MissionWorkbench::with_client(WorkspaceClient::for_state_dir(
            std::env::temp_dir().join(format!("jterm-mission-ui-{}", uuid::Uuid::new_v4())),
        ))
    }

    #[test]
    fn a_late_read_cannot_replace_a_new_stable_selection() {
        let mut state = workbench();
        let first = WorkspaceMission::new("First", "First acceptance");
        let second = WorkspaceMission::new("Second", "Second acceptance");
        state.selected = Some(second.clone());
        state.generation = 2;
        state.pending.insert(
            1,
            Pending::Read {
                id: first.id.clone(),
                generation: 1,
            },
        );
        state.receive(
            1,
            Ok(WorkspaceReply::Mission {
                mission: first,
                report: None,
            }),
        );
        assert_eq!(state.selected, Some(second));
    }

    #[test]
    fn candidate_adoption_rejects_dirty_input_without_saving() {
        let mut state = workbench();
        let mut mission = WorkspaceMission::new("First", "Acceptance");
        let repository = WorkspaceMissionRepository::new("example/repo", "/tmp/repo").unwrap();
        let evidence = WorkspaceMissionRepositoryEvidence {
            identity: repository.identity.clone(),
            path: repository.path.clone(),
            local: Some(WorkspaceMissionLocalEvidence {
                baseline: "a".repeat(40),
                head: "b".repeat(40),
                fingerprint: "dirty-input".into(),
                modified: true,
                dirty: true,
                manifest_core_pin: None,
                lock_core_pin: None,
                observed_at_ms: 1,
            }),
            pushed: None,
            integrated: None,
            validations: Vec::new(),
            error: None,
        };
        mission.repositories.push(repository);
        state.report = Some(mission.project(&[evidence]));
        state.selected = Some(mission);
        state.adopt_candidates();
        assert!(state.notice.as_ref().unwrap().contains("dirty input"));
        assert!(state.selected.as_ref().unwrap().repositories[0]
            .candidate
            .is_none());
        assert_ne!(
            state.report.as_ref().unwrap().state,
            WorkspaceMissionState::Completed
        );
        assert!(state.pending.is_empty());
    }

    #[test]
    fn catalog_refresh_invalidates_a_report_from_another_revision() {
        let mut state = workbench();
        let mut mission = WorkspaceMission::new("Release", "Acceptance");
        mission.revision = 1;
        state.report = Some(mission.project(&[]));
        state.selected = Some(mission.clone());
        mission.revision = 2;
        mission.goal = "Changed acceptance".into();
        state.pending.insert(1, Pending::Catalog);
        state.receive(
            1,
            Ok(WorkspaceReply::Missions {
                missions: vec![mission.clone()],
            }),
        );
        assert_eq!(state.selected, Some(mission));
        assert!(state.report.is_none());
    }
}
