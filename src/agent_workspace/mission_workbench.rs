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
        evidence_generation: u64,
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
    evidence_generation: u64,
    candidate_inspection: Option<(String, u64)>,
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
            evidence_generation: 0,
            candidate_inspection: None,
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
        self.candidate_inspection = None;
        self.evidence_generation = self.evidence_generation.wrapping_add(1);
        // A catalog reload is not an observation of the repositories. Do not
        // let an unchanged plan revision keep an externally changed candidate
        // green in the attention count or the selected report.
        for report in self.reports.values_mut() {
            report.require_reinspection();
        }
        if let Some(report) = &mut self.report {
            report.require_reinspection();
        }
        if !self
            .pending
            .values()
            .any(|value| matches!(value, Pending::Catalog))
        {
            self.send(WorkspaceCommand::MissionList, Pending::Catalog);
        }
    }

    pub fn select(&mut self, id: &str) {
        self.candidate_inspection = None;
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
        self.candidate_inspection = None;
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
        self.candidate_inspection = None;
        self.evidence_generation = self.evidence_generation.wrapping_add(1);
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
        self.candidate_inspection = None;
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
                evidence_generation: self.evidence_generation,
            },
        );
    }

    /// Explicit user action: freeze observed commits, preserving the original
    /// baseline after its first capture. Changing candidates invalidates reports.
    pub fn adopt_candidates(&mut self) {
        let inspection = self.candidate_inspection.take();
        let (Some(mut mission), Some(report)) = (self.selected.clone(), self.report.as_ref())
        else {
            return;
        };
        if self.busy || report.id != mission.id || report.revision != mission.revision {
            return;
        }
        if mission.repositories.is_empty() {
            self.notice = Some("Add repositories before freezing delivery candidates".into());
            return;
        }
        if inspection.as_ref() != Some(&(mission.id.clone(), mission.revision)) {
            self.notice = Some("Inspect the current Mission before freezing candidates; saved observations are historical, not permission to adopt them".into());
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

    /// Only a successful Inspect reply since the latest selection, read,
    /// refresh or save can offer candidates. This is a dated observation, not
    /// a filesystem lock; callers must not infer authority from a saved report.
    pub fn can_adopt_candidates(&self) -> bool {
        let (Some(mission), Some(report)) = (&self.selected, &self.report) else {
            return false;
        };
        !self.busy
            && report.id == mission.id
            && report.revision == mission.revision
            && self.candidate_inspection.as_ref() == Some(&(mission.id.clone(), mission.revision))
            && !mission.repositories.is_empty()
            && mission.repositories.iter().all(|repository| {
                report
                    .repositories
                    .iter()
                    .find(|entry| entry.identity.matches(&repository.identity))
                    .and_then(|entry| entry.observed.as_ref())
                    .is_some_and(|observed| !observed.dirty)
            })
    }

    pub fn link_conversation(&mut self, id: &ConversationId) {
        self.candidate_inspection = None;
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
        self.candidate_inspection = None;
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

    /// Read a cached observation for this exact saved revision. The report's
    /// gates already reflect refresh/reopen invalidation; this never inspects
    /// repositories, starts a host or changes the stable selected Mission.
    pub fn report_for(&self, id: &str) -> Option<&WorkspaceMissionReport> {
        let mission = self.missions.iter().find(|mission| mission.id == id)?;
        self.reports
            .get(id)
            .filter(|report| report.id == id && report.revision == mission.revision)
    }

    /// Uninspected plans stay visible as requiring review, never presumed done.
    pub fn needs_attention(&self) -> usize {
        self.missions
            .iter()
            .filter(|mission| {
                self.report_for(&mission.id)
                    .is_none_or(WorkspaceMissionReport::needs_you)
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
        if matches!(&pending, Pending::Inspect { evidence_generation, .. }
            if *evidence_generation != self.evidence_generation)
        {
            return;
        }
        let saved_read = matches!(&pending, Pending::Read { .. } | Pending::Reconcile { .. });
        match pending {
            Pending::Catalog => match result {
                Ok(WorkspaceReply::Missions { missions }) => {
                    if let Some(selected) = &self.selected {
                        if let Some(current) =
                            missions.iter().find(|mission| mission.id == selected.id)
                        {
                            if current.revision != selected.revision {
                                self.candidate_inspection = None;
                                self.selected = Some(current.clone());
                                self.report = None;
                                self.notice = Some("The saved plan changed in another view. Review the current revision; your local draft remains separate.".into());
                            }
                        } else if selected.revision > 0 {
                            self.candidate_inspection = None;
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
            Pending::Read { id, generation } | Pending::Inspect { id, generation, .. } => {
                if generation != self.generation {
                    return;
                }
                match result {
                    Ok(WorkspaceReply::Mission {
                        mission,
                        mut report,
                    }) if mission.id == id => {
                        // An already-running older host may still replay its
                        // saved report. The updated client must also fail closed.
                        if saved_read {
                            self.evidence_generation = self.evidence_generation.wrapping_add(1);
                            if let Some(report) = &mut report {
                                report.require_reinspection();
                            }
                        }
                        let inspected = if saved_read {
                            None
                        } else {
                            report
                                .as_ref()
                                .filter(|report| {
                                    report.id == mission.id && report.revision == mission.revision
                                })
                                .map(|_| (mission.id.clone(), mission.revision))
                        };
                        self.apply(mission, report);
                        self.candidate_inspection = inspected;
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
                    Ok(WorkspaceReply::Mission {
                        mission,
                        mut report,
                    }) if mission.id == attempt.id => {
                        self.evidence_generation = self.evidence_generation.wrapping_add(1);
                        if let Some(report) = &mut report {
                            report.require_reinspection();
                        }
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
        self.candidate_inspection = None;
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
        state.candidate_inspection = Some((mission.id.clone(), mission.revision));
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

    #[test]
    fn catalog_refresh_is_a_barrier_to_an_older_inspection_reply() {
        let mut state = workbench();
        let mut mission = WorkspaceMission::new("Release", "Acceptance");
        mission.revision = 1;
        let mut completed = mission.project(&[]);
        completed.state = WorkspaceMissionState::Completed;
        state.selected = Some(mission.clone());
        state.missions = vec![mission.clone()];
        state.pending.insert(
            100,
            Pending::Inspect {
                id: mission.id.clone(),
                generation: state.generation,
                evidence_generation: state.evidence_generation,
            },
        );
        // An existing catalog request prevents this unit test from sending
        // another host request; refresh must still advance the evidence epoch.
        state.pending.insert(101, Pending::Catalog);
        state.refresh();
        state.receive(
            100,
            Ok(WorkspaceReply::Mission {
                mission,
                report: Some(completed),
            }),
        );
        assert!(state.report.is_none());
        assert!(state.reports.is_empty());
        assert_eq!(state.needs_attention(), 1);
    }

    #[test]
    fn old_host_saved_reads_cannot_restore_current_success() {
        for reconcile in [false, true] {
            let mut state = workbench();
            let mut mission = WorkspaceMission::new("Release", "Acceptance");
            mission.revision = 1;
            mission
                .repositories
                .push(WorkspaceMissionRepository::new("example/repo", "/tmp/repo").unwrap());
            let mut saved = mission.project(&[]);
            saved.state = WorkspaceMissionState::Completed;
            saved.repositories[0].validated = true;
            saved.repositories[0].checks[0].status = WorkspaceMissionCheckStatus::Passed;
            state.selected = Some(mission.clone());
            let pending = if reconcile {
                Pending::Reconcile {
                    mission: mission.clone(),
                    generation: state.generation,
                }
            } else {
                Pending::Read {
                    id: mission.id.clone(),
                    generation: state.generation,
                }
            };
            state.pending.insert(100, pending);
            state.pending.insert(101, Pending::Catalog);
            state.receive(
                100,
                Ok(WorkspaceReply::Mission {
                    mission,
                    report: Some(saved),
                }),
            );
            let report = state.report.as_ref().unwrap();
            assert_eq!(report.state, WorkspaceMissionState::NeedsAttention);
            assert!(!report.repositories[0].validated);
            assert_eq!(
                report.repositories[0].checks[0].status,
                WorkspaceMissionCheckStatus::Stale
            );
            assert_eq!(state.needs_attention(), 1);
        }
    }

    #[test]
    fn per_mission_reports_require_the_catalog_revision_without_changing_selection() {
        let mut state = workbench();
        let mut mission = WorkspaceMission::new("Release", "Acceptance");
        mission.revision = 1;
        state.missions.push(mission.clone());
        state
            .reports
            .insert(mission.id.clone(), mission.project(&[]));
        assert!(state.report_for(&mission.id).is_some());
        assert!(state.report_for("missing").is_none());
        assert!(state.selected.is_none());
        state.missions[0].revision = 2;
        assert!(state.report_for(&mission.id).is_none());
        assert!(state.pending.is_empty());
        assert_eq!(state.needs_attention(), 1);
    }

    fn clean_candidate() -> (WorkspaceMission, WorkspaceMissionReport) {
        let mut mission = WorkspaceMission::new("Release", "Acceptance");
        mission.revision = 1;
        let repository = WorkspaceMissionRepository::new("example/repo", "/tmp/repo").unwrap();
        let evidence = WorkspaceMissionRepositoryEvidence {
            identity: repository.identity.clone(),
            path: repository.path.clone(),
            local: Some(WorkspaceMissionLocalEvidence {
                baseline: "a".repeat(40),
                head: "b".repeat(40),
                fingerprint: "clean-input".into(),
                modified: true,
                dirty: false,
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
        let report = mission.project(&[evidence]);
        (mission, report)
    }

    fn inspection(state: &MissionWorkbench, mission: &WorkspaceMission) -> Pending {
        Pending::Inspect {
            id: mission.id.clone(),
            generation: state.generation,
            evidence_generation: state.evidence_generation,
        }
    }

    #[test]
    fn only_an_explicit_current_inspection_can_offer_candidate_adoption() {
        let mut empty = workbench();
        let mission = WorkspaceMission::new("Empty", "Acceptance");
        let report = mission.project(&[]);
        empty.pending.insert(100, inspection(&empty, &mission));
        empty.receive(
            100,
            Ok(WorkspaceReply::Mission {
                mission,
                report: Some(report),
            }),
        );
        assert!(!empty.can_adopt_candidates());
        empty.adopt_candidates();
        assert!(
            empty.pending.is_empty(),
            "an empty Mission must not be saved by adoption"
        );

        let mut state = workbench();
        let (mission, report) = clean_candidate();
        assert_eq!(report.state, WorkspaceMissionState::NeedsAttention);
        state.apply(mission.clone(), Some(report.clone()));
        assert!(!state.can_adopt_candidates());
        state.adopt_candidates();
        assert!(state
            .notice
            .as_ref()
            .unwrap()
            .contains("Inspect the current Mission"));
        assert!(state.pending.is_empty());
        state.pending.insert(100, inspection(&state, &mission));
        state.receive(
            100,
            Ok(WorkspaceReply::Mission {
                mission,
                report: Some(report),
            }),
        );
        assert!(
            state.can_adopt_candidates(),
            "a fresh pre-freeze report legitimately needs attention"
        );
        state.adopt_candidates();
        assert!(!state.can_adopt_candidates());
        assert!(state
            .pending
            .values()
            .any(|pending| matches!(pending, Pending::Write { mission, .. }
            if mission.repositories[0].candidate.as_deref() == Some("b".repeat(40).as_str()))));
    }

    #[test]
    fn refresh_select_save_and_read_revoke_adoption_and_prior_inspection_replies() {
        for action in 0..4 {
            let mut state = workbench();
            let (mission, report) = clean_candidate();
            state.pending.insert(100, inspection(&state, &mission));
            state.receive(
                100,
                Ok(WorkspaceReply::Mission {
                    mission: mission.clone(),
                    report: Some(report.clone()),
                }),
            );
            assert!(state.can_adopt_candidates());
            state.pending.insert(101, inspection(&state, &mission));
            match action {
                0 => {
                    state.pending.insert(102, Pending::Catalog);
                    state.refresh();
                }
                1 => state.select(&mission.id),
                2 => {
                    let mut invalid = mission.clone();
                    invalid.title.clear();
                    state.save(invalid);
                }
                _ => {
                    state.pending.insert(
                        102,
                        Pending::Read {
                            id: mission.id.clone(),
                            generation: state.generation,
                        },
                    );
                    state.receive(
                        102,
                        Ok(WorkspaceReply::Mission {
                            mission: mission.clone(),
                            report: Some(report.clone()),
                        }),
                    );
                }
            }
            assert!(!state.can_adopt_candidates());
            state.receive(
                101,
                Ok(WorkspaceReply::Mission {
                    mission: mission.clone(),
                    report: Some(report.clone()),
                }),
            );
            assert!(
                state.candidate_inspection.is_none(),
                "an older Inspect reply cannot restore adoption after action {action}"
            );
            assert!(!state.can_adopt_candidates());
        }
    }
}
