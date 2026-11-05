//! A small, declarative delivery bundle over existing workspace tasks.
//!
//! A mission is not a scheduler and owns no process or conversation. Plans are
//! client-editable; evidence passed to `project` must come from the host's Git,
//! validation or CI inspector. Deserializing an evidence record does not verify
//! its provenance. In particular, SaveMission must never accept these records,
//! and model output, model names and a successful provider turn are not evidence.

use super::{ConversationId, WorkspaceResultReport};
use crate::agent_task::{TaskId, TaskValidationStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

pub const WORKSPACE_MISSION_REPOSITORIES: usize = 16;
pub const WORKSPACE_MISSION_CHECKS: usize = 32;
pub const WORKSPACE_MISSION_TASK_REFS: usize = 32;

pub(super) const REINSPECTION_DETAIL: &str =
    "Saved observation requires reinspection of the current candidate, dependencies and target ref";

/// Repository IDs are hints for detecting mismatches, not proof of authority.
pub const FOUR_TERMINAL_REPOSITORIES: [(&str, u64); 5] = [
    ("beamiter/jterm_core", 1_312_049_979),
    ("beamiter/anvil", 1_270_697_740),
    ("beamiter/forge", 1_170_023_936),
    ("beamiter/ember", 1_197_022_383),
    ("beamiter/frost", 1_259_573_969),
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMissionRepositoryIdentity {
    pub full_name: String,
    /// Zero means unknown. A number alone never establishes remote provenance.
    #[serde(default)]
    pub github_id: u64,
}

impl WorkspaceMissionRepositoryIdentity {
    pub fn canonical(full_name: &str) -> Result<Self, String> {
        let full_name = full_name.to_ascii_lowercase();
        let github_id = FOUR_TERMINAL_REPOSITORIES
            .iter()
            .find(|(name, _)| *name == full_name)
            .map_or(0, |(_, id)| *id);
        let identity = Self {
            full_name,
            github_id,
        };
        identity.validate()?;
        Ok(identity)
    }

    pub fn validate(&self) -> Result<(), String> {
        let parts: Vec<_> = self.full_name.split('/').collect();
        if self.full_name.len() > 256
            || parts.len() != 2
            || parts.iter().any(|part| {
                part.is_empty()
                    || matches!(*part, "." | "..")
                    || !part
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            })
        {
            return Err("A repository requires its exact owner/name identity".into());
        }
        if let Some((_, known)) = FOUR_TERMINAL_REPOSITORIES
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&self.full_name))
        {
            if self.github_id != 0 && self.github_id != *known {
                return Err(format!("Repository ID mismatch for {}", self.full_name));
            }
        }
        Ok(())
    }

    pub fn matches(&self, other: &Self) -> bool {
        self.full_name.eq_ignore_ascii_case(&other.full_name)
            && (self.github_id == 0 || other.github_id == 0 || self.github_id == other.github_id)
    }
}

/// References the exact existing task and host run; it creates neither.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMissionTaskRef {
    pub conversation: ConversationId,
    pub task_id: TaskId,
    pub run_id: String,
    pub source_sequence: u64,
}

impl WorkspaceMissionTaskRef {
    pub fn from_result(report: &WorkspaceResultReport) -> Self {
        Self {
            conversation: report.conversation.clone(),
            task_id: report.task_id,
            run_id: report.run_id.clone(),
            source_sequence: report.source_sequence,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let parts = self
            .conversation
            .0
            .strip_prefix("jterm:task:v1:")
            .filter(|_| self.conversation.0.len() <= 32768)
            .and_then(|text| serde_json::from_str::<(String, String, String)>(text).ok());
        if !parts.is_some_and(|(host, namespace, task)| {
            !host.is_empty()
                && !namespace.is_empty()
                && task == self.task_id.to_string()
                && !host.contains('\0')
                && !namespace.contains('\0')
        }) {
            return Err("Mission task reference must match its source-qualified TaskId".into());
        }
        visible(&self.run_id, 256, "Task run identity")
    }

    fn covers(&self, required: &Self) -> bool {
        self.conversation == required.conversation
            && self.task_id == required.task_id
            && self.run_id == required.run_id
            && self.source_sequence >= required.source_sequence
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMissionRepository {
    pub identity: WorkspaceMissionRepositoryIdentity,
    pub path: PathBuf,
    /// Empty only while the delivery plan is still a draft.
    pub baseline: String,
    pub candidate: Option<String>,
    pub target_ref: String,
    pub required_core_pin: Option<String>,
    #[serde(default)]
    pub tasks: Vec<WorkspaceMissionTaskRef>,
    #[serde(default)]
    pub required_checks: Vec<String>,
}

impl WorkspaceMissionRepository {
    pub fn new(full_name: &str, path: impl Into<PathBuf>) -> Result<Self, String> {
        Ok(Self {
            identity: WorkspaceMissionRepositoryIdentity::canonical(full_name)?,
            path: path.into(),
            baseline: String::new(),
            candidate: None,
            target_ref: "refs/heads/master".into(),
            required_core_pin: None,
            tasks: Vec::new(),
            required_checks: vec!["validation".into()],
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        self.identity.validate()?;
        valid_path(&self.path)?;
        if !self.baseline.is_empty() {
            valid_commit(&self.baseline)?;
        }
        if let Some(commit) = &self.candidate {
            valid_commit(commit)?;
        }
        if let Some(commit) = &self.required_core_pin {
            valid_commit(commit)?;
        }
        valid_ref(&self.target_ref)?;
        if self.tasks.len() > WORKSPACE_MISSION_TASK_REFS {
            return Err("A repository has too many linked task runs".into());
        }
        let mut seen = BTreeSet::new();
        for task in &self.tasks {
            task.validate()?;
            if !seen.insert((&task.conversation, &task.run_id)) {
                return Err("Mission task run references must be unique".into());
            }
        }
        unique_checks(&self.required_checks)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMissionPreset {
    #[default]
    Custom,
    FourTerminals,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMission {
    pub id: String,
    pub revision: u64,
    pub title: String,
    pub goal: String,
    pub required_environment: String,
    pub updated_at_ms: u64,
    /// Host-owned acceptance-contract boundary; client-provided values are ignored.
    #[serde(default)]
    pub evidence_after_ms: u64,
    #[serde(default)]
    pub preset: WorkspaceMissionPreset,
    pub repositories: Vec<WorkspaceMissionRepository>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMissionUpdate {
    pub expected_revision: u64,
    pub mission: WorkspaceMission,
}

impl WorkspaceMission {
    pub fn new_id() -> String {
        format!("mission:{}", uuid::Uuid::new_v4())
    }

    pub fn new(title: impl Into<String>, goal: impl Into<String>) -> Self {
        Self {
            id: Self::new_id(),
            revision: 0,
            title: title.into(),
            goal: goal.into(),
            required_environment: "local".into(),
            updated_at_ms: 0,
            evidence_after_ms: 0,
            preset: WorkspaceMissionPreset::Custom,
            repositories: Vec::new(),
        }
    }

    /// Paths and unresolved commits remain visible draft inputs. No filesystem
    /// access, branch creation, validation or remote operation happens here.
    pub fn four_terminals(
        title: impl Into<String>,
        goal: impl Into<String>,
        parent: impl AsRef<Path>,
    ) -> Self {
        let mut mission = Self::new(title, goal);
        mission.preset = WorkspaceMissionPreset::FourTerminals;
        mission.repositories = FOUR_TERMINAL_REPOSITORIES
            .iter()
            .map(|(name, _)| {
                WorkspaceMissionRepository::new(
                    name,
                    parent.as_ref().join(name.split('/').nth(1).unwrap()),
                )
                .expect("the built-in repository identities are valid")
            })
            .collect();
        mission
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty()
            || self.id.len() > 128
            || !self
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_:".contains(&c))
        {
            return Err("Mission identity must be a bounded stable key".into());
        }
        visible(&self.title, 256, "Mission title")?;
        visible(&self.goal, 16 * 1024, "Mission acceptance goal")?;
        visible(
            &self.required_environment,
            512,
            "Required validation environment",
        )?;
        if self.repositories.len() > WORKSPACE_MISSION_REPOSITORIES {
            return Err("A mission has too many repositories".into());
        }
        let mut names = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for repo in &self.repositories {
            repo.validate()?;
            if !names.insert(repo.identity.full_name.to_ascii_lowercase()) {
                return Err("A mission must declare each repository once".into());
            }
            if !paths.insert(normal_path(&repo.path)) {
                return Err("Different mission repositories cannot share one path".into());
            }
        }
        if self.preset == WorkspaceMissionPreset::FourTerminals
            && (names.len() != FOUR_TERMINAL_REPOSITORIES.len()
                || FOUR_TERMINAL_REPOSITORIES
                    .iter()
                    .any(|(name, _)| !names.contains(*name)))
        {
            return Err(
                "The four-terminal preset requires jterm_core, anvil, forge, ember and frost"
                    .into(),
            );
        }
        Ok(())
    }

    /// Pure CAS preparation; the caller must read and persist it in the same
    /// SQLite writer transaction. Revision zero is a new unsaved mission.
    pub fn apply_update(
        &self,
        update: &WorkspaceMissionUpdate,
        now_ms: u64,
    ) -> Result<Self, String> {
        if self.id != update.mission.id {
            return Err("Mission identity cannot change during an update".into());
        }
        if self.revision != update.expected_revision
            || update.mission.revision != update.expected_revision
        {
            return Err(format!(
                "Mission revision conflict: expected {}, current {}",
                update.expected_revision, self.revision
            ));
        }
        update.mission.validate()?;
        let mut next = update.mission.clone();
        next.revision = self
            .revision
            .checked_add(1)
            .ok_or("Mission revision exhausted")?;
        next.updated_at_ms = now_ms;
        Ok(next)
    }

    /// Evidence is supplied exclusively by the trusted host inspector. Saved
    /// plans cannot assert successful stages or manufacture completion.
    pub fn project(
        &self,
        evidence: &[WorkspaceMissionRepositoryEvidence],
    ) -> WorkspaceMissionReport {
        let mut report = WorkspaceMissionReport {
            id: self.id.clone(),
            revision: self.revision,
            state: WorkspaceMissionState::InProgress,
            repositories: Vec::new(),
            errors: Vec::new(),
        };
        if let Err(error) = self.validate() {
            report.errors.push(error);
        }
        for observed in evidence {
            if !self
                .repositories
                .iter()
                .any(|repo| repo.identity.matches(&observed.identity))
            {
                report.errors.push(format!(
                    "Evidence repository mismatch: {} is not declared",
                    observed.identity.full_name
                ));
            }
        }
        let core_candidate = self
            .repositories
            .iter()
            .find(|repo| {
                repo.identity
                    .full_name
                    .eq_ignore_ascii_case("beamiter/jterm_core")
            })
            .and_then(|repo| repo.candidate.as_deref());
        let declares_core = self.repositories.iter().any(|repo| {
            repo.identity
                .full_name
                .eq_ignore_ascii_case("beamiter/jterm_core")
        });
        for repo in &self.repositories {
            let matches: Vec<_> = evidence
                .iter()
                .filter(|entry| {
                    repo.identity
                        .full_name
                        .eq_ignore_ascii_case(&entry.identity.full_name)
                })
                .collect();
            let mut projected =
                project_repository(repo, matches.first().copied(), &self.required_environment);
            if matches.len() > 1 {
                projected
                    .errors
                    .push("Ambiguous duplicate repository evidence".into());
            }
            let terminal = FOUR_TERMINAL_REPOSITORIES[1..]
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(&repo.identity.full_name));
            if ((self.preset == WorkspaceMissionPreset::FourTerminals && terminal)
                || (declares_core && repo.required_core_pin.is_some()))
                && (core_candidate.is_none() || repo.required_core_pin.as_deref() != core_candidate)
            {
                projected.pins_current = false;
                projected
                    .errors
                    .push("The terminal must pin this mission's exact jterm_core candidate".into());
            }
            report.repositories.push(projected);
        }
        let integrated = report
            .repositories
            .iter()
            .filter(|repo| repo.integrated)
            .count();
        report.state = if !report.errors.is_empty() {
            WorkspaceMissionState::NeedsAttention
        } else if self.repositories.is_empty() {
            WorkspaceMissionState::Draft
        } else if report
            .repositories
            .iter()
            .all(WorkspaceMissionRepositoryReport::complete)
        {
            WorkspaceMissionState::Completed
        } else if integrated > 0 && integrated < report.repositories.len() {
            WorkspaceMissionState::PartiallyIntegrated
        } else if report
            .repositories
            .iter()
            .any(|repo| !repo.errors.is_empty())
        {
            WorkspaceMissionState::NeedsAttention
        } else {
            WorkspaceMissionState::InProgress
        };
        report
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMissionLocalEvidence {
    pub baseline: String,
    /// The exact commit observed by Git, compared with the planned candidate.
    pub head: String,
    pub fingerprint: String,
    pub modified: bool,
    pub dirty: bool,
    pub manifest_core_pin: Option<String>,
    pub lock_core_pin: Option<String>,
    pub observed_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMissionRemoteEvidence {
    pub repository: WorkspaceMissionRepositoryIdentity,
    pub ref_name: String,
    pub head: String,
    pub observed_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMissionIntegrationEvidence {
    pub remote: WorkspaceMissionRemoteEvidence,
    pub candidate: String,
    /// Set only from the host's ancestry check against the observed remote ref.
    pub contains_candidate: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMissionValidationSource {
    Workspace,
    Ci,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMissionValidationEvidence {
    pub check_name: String,
    pub environment: String,
    pub candidate: String,
    pub fingerprint: String,
    pub status: TaskValidationStatus,
    pub task: Option<WorkspaceMissionTaskRef>,
    pub source: WorkspaceMissionValidationSource,
    /// A lookup key in the host's evidence store, not free-form provider output.
    pub receipt_id: String,
    pub completed_at_ms: Option<u64>,
}

impl WorkspaceMissionValidationEvidence {
    /// Bind the existing result identity and validation fingerprint. The caller
    /// obtains status from the exact task snapshot, never the report's UI label.
    pub fn from_result(
        check_name: impl Into<String>,
        environment: impl Into<String>,
        report: &WorkspaceResultReport,
        status: TaskValidationStatus,
    ) -> Result<Self, String> {
        let current = report
            .current
            .as_ref()
            .ok_or("Current worktree evidence is unavailable")?;
        let evidence = report
            .validation
            .as_ref()
            .ok_or("Validation evidence is unavailable")?;
        if report.error.is_some()
            || !report.validation_current
            || !evidence.unchanged_after_run
            || evidence.fingerprint != current.fingerprint
            || evidence.head != current.head
            || evidence.base_commit != current.base_commit
            || evidence.command.trim().is_empty()
            || evidence
                .completed_at_ms
                .is_none_or(|end| end < evidence.started_at_ms)
        {
            return Err("Validation is stale, incomplete or not bound to this worktree".into());
        }
        let task = WorkspaceMissionTaskRef::from_result(report);
        task.validate()?;
        let receipt_id = format!("workspace:{}:{}", task.run_id, task.source_sequence);
        let receipt = Self {
            check_name: check_name.into(),
            environment: environment.into(),
            candidate: current.head.clone(),
            fingerprint: current.fingerprint.clone(),
            status,
            task: Some(task),
            source: WorkspaceMissionValidationSource::Workspace,
            receipt_id,
            completed_at_ms: evidence.completed_at_ms,
        };
        receipt.validate()?;
        Ok(receipt)
    }

    pub fn validate(&self) -> Result<(), String> {
        visible(&self.check_name, 128, "Validation check name")?;
        visible(&self.environment, 512, "Validation environment")?;
        valid_commit(&self.candidate)?;
        visible(&self.fingerprint, 256, "Validation fingerprint")?;
        visible(&self.receipt_id, 1024, "Host validation receipt")?;
        if let Some(task) = &self.task {
            task.validate()?;
        }
        if self.source == WorkspaceMissionValidationSource::Workspace && self.task.is_none() {
            return Err("Workspace validation must reference its exact task run".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMissionRepositoryEvidence {
    pub identity: WorkspaceMissionRepositoryIdentity,
    pub path: PathBuf,
    pub local: Option<WorkspaceMissionLocalEvidence>,
    pub pushed: Option<WorkspaceMissionRemoteEvidence>,
    pub integrated: Option<WorkspaceMissionIntegrationEvidence>,
    #[serde(default)]
    pub validations: Vec<WorkspaceMissionValidationEvidence>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMissionState {
    Draft,
    InProgress,
    PartiallyIntegrated,
    NeedsAttention,
    Completed,
}

impl WorkspaceMissionState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Draft => "Draft",
            Self::InProgress => "Delivery in progress",
            Self::PartiallyIntegrated => "Partially integrated",
            Self::NeedsAttention => "Needs your attention",
            Self::Completed => "Mission completed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMissionCheckStatus {
    Passed,
    Failed,
    Blocked,
    NotRun,
    Stale,
}

impl WorkspaceMissionCheckStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Passed => "Passed",
            Self::Failed => "Failed",
            Self::Blocked => "Blocked",
            Self::NotRun => "Not run",
            Self::Stale => "Stale",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMissionCheckReport {
    pub check_name: String,
    pub environment: String,
    pub status: WorkspaceMissionCheckStatus,
    pub receipt_id: Option<String>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMissionRepositoryReport {
    pub identity: WorkspaceMissionRepositoryIdentity,
    pub path: PathBuf,
    pub candidate: Option<String>,
    /// The host's last local capture, including the SHA offered for adoption.
    pub observed: Option<WorkspaceMissionLocalEvidence>,
    pub modified: bool,
    pub committed: bool,
    pub pushed: bool,
    pub integrated: bool,
    /// Dated host observations remain available after current gates expire.
    #[serde(default)]
    pub pushed_observation: Option<WorkspaceMissionRemoteEvidence>,
    #[serde(default)]
    pub integration_observation: Option<WorkspaceMissionIntegrationEvidence>,
    pub validated: bool,
    pub pins_current: bool,
    #[serde(default)]
    pub checks: Vec<WorkspaceMissionCheckReport>,
    pub covered_checks: Vec<String>,
    pub missing_checks: Vec<String>,
    pub errors: Vec<String>,
}

impl WorkspaceMissionRepositoryReport {
    pub fn complete(&self) -> bool {
        self.observed.is_some()
            && self.committed
            && self.pushed
            && self.integrated
            && self.validated
            && self.pins_current
            && self.errors.is_empty()
    }

    /// This bundle never starts, validates or delivers automatically. Any
    /// unfinished gate still needs an explicit next action from the workbench.
    pub fn needs_you(&self) -> bool {
        !self.complete()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMissionReport {
    pub id: String,
    pub revision: u64,
    pub state: WorkspaceMissionState,
    pub repositories: Vec<WorkspaceMissionRepositoryReport>,
    pub errors: Vec<String>,
}

impl WorkspaceMissionReport {
    /// Saved reports are dated observations. Reopening them cannot establish
    /// that another process has not changed a checkout, source or remote ref.
    /// Preserve the observation and receipt links, but require a fresh read
    /// before presenting any delivery gate as currently satisfied.
    pub fn require_reinspection(&mut self) {
        const DETAIL: &str = REINSPECTION_DETAIL;
        if !self.repositories.is_empty() {
            self.state = WorkspaceMissionState::NeedsAttention;
        }
        for repository in &mut self.repositories {
            repository.committed = false;
            repository.pushed = false;
            repository.integrated = false;
            repository.validated = false;
            repository.pins_current = false;
            repository.covered_checks.clear();
            repository.missing_checks = repository
                .checks
                .iter()
                .map(|check| check.check_name.clone())
                .collect();
            for check in &mut repository.checks {
                if check.status != WorkspaceMissionCheckStatus::NotRun {
                    check.status = WorkspaceMissionCheckStatus::Stale;
                    check.detail = Some(DETAIL.into());
                }
            }
            if !repository.errors.iter().any(|error| error == DETAIL) {
                repository.errors.push(DETAIL.into());
            }
        }
    }

    pub fn needs_you(&self) -> bool {
        !self.errors.is_empty()
            || self.repositories.is_empty()
            || self
                .repositories
                .iter()
                .any(WorkspaceMissionRepositoryReport::needs_you)
    }
    pub fn needs_attention(&self) -> bool {
        self.needs_you()
    }
}

fn project_repository(
    repo: &WorkspaceMissionRepository,
    observed: Option<&WorkspaceMissionRepositoryEvidence>,
    environment: &str,
) -> WorkspaceMissionRepositoryReport {
    let mut report = WorkspaceMissionRepositoryReport {
        identity: repo.identity.clone(),
        path: repo.path.clone(),
        candidate: repo.candidate.clone(),
        observed: None,
        modified: false,
        committed: false,
        pushed: false,
        integrated: false,
        pushed_observation: None,
        integration_observation: None,
        validated: false,
        pins_current: repo.required_core_pin.is_none(),
        covered_checks: Vec::new(),
        checks: repo
            .required_checks
            .iter()
            .map(|name| WorkspaceMissionCheckReport {
                check_name: name.clone(),
                environment: environment.into(),
                status: WorkspaceMissionCheckStatus::Blocked,
                receipt_id: None,
                detail: Some("A matching repository snapshot and candidate are required".into()),
            })
            .collect(),
        missing_checks: repo.required_checks.clone(),
        errors: Vec::new(),
    };
    let Some(observed) = observed else {
        report
            .errors
            .push("Repository evidence is unavailable; absence is not completion".into());
        return report;
    };
    if observed.identity.validate().is_err()
        || !repo.identity.matches(&observed.identity)
        || normal_path(&repo.path) != normal_path(&observed.path)
    {
        report
            .errors
            .push("Repository identity or path does not match the declared delivery".into());
        return report;
    }
    if let Some(error) = &observed.error {
        report.errors.push(error.clone());
    }
    let Some(local) = &observed.local else {
        report
            .errors
            .push("Local repository evidence is unavailable".into());
        return report;
    };
    report.observed = Some(local.clone());
    report.modified = local.modified;
    let Some(candidate) = repo.candidate.as_deref() else {
        report
            .errors
            .push("Select an exact candidate commit before delivery".into());
        return report;
    };
    if repo.baseline.is_empty() || local.baseline != repo.baseline {
        report
            .errors
            .push("Repository baseline is missing or does not match captured evidence".into());
    }
    if valid_commit(&local.head).is_err() || local.head != candidate || local.fingerprint.is_empty()
    {
        report
            .errors
            .push("Local evidence is stale or does not identify the candidate commit".into());
    }
    report.committed = local.head == candidate && valid_commit(candidate).is_ok() && !local.dirty;
    if local.dirty {
        report
            .errors
            .push("Worktree contains changes outside the candidate commit".into());
    }
    if let Some(pin) = &repo.required_core_pin {
        report.pins_current = local.manifest_core_pin.as_ref() == Some(pin)
            && local.lock_core_pin.as_ref() == Some(pin);
        if !report.pins_current {
            report
                .errors
                .push("Manifest and lockfile do not both pin the required core commit".into());
        }
    }
    if let Some(remote) = &observed.pushed {
        report.pushed_observation = Some(remote.clone());
        let reachable = observed.integrated.as_ref().is_some_and(|integrated| {
            integrated.remote == *remote
                && integrated.candidate == candidate
                && integrated.contains_candidate
        });
        report.pushed =
            remote_matches(&repo.identity, remote) && (remote.head == candidate || reachable);
        if !report.pushed {
            report
                .errors
                .push("Push evidence does not match the repository and candidate commit".into());
        }
    }
    if let Some(integrated) = &observed.integrated {
        report.integration_observation = Some(integrated.clone());
        report.integrated = remote_matches(&repo.identity, &integrated.remote)
            && integrated.remote.ref_name == repo.target_ref
            && integrated.candidate == candidate
            && integrated.contains_candidate;
        if !report.integrated {
            report.errors.push(
                "Integration evidence does not establish candidate ancestry on the target ref"
                    .into(),
            );
        }
    }
    if repo.required_checks.is_empty() {
        report
            .errors
            .push("Declare at least one acceptance check before completing a mission".into());
    }
    report.checks.clear();
    for required in &repo.required_checks {
        let mut check_report = WorkspaceMissionCheckReport {
            check_name: required.clone(),
            environment: environment.into(),
            status: WorkspaceMissionCheckStatus::NotRun,
            receipt_id: None,
            detail: Some("No host validation receipt is available for this check".into()),
        };
        let matching: Vec<_> = observed
            .validations
            .iter()
            .filter(|check| &check.check_name == required)
            .collect();
        let current: Vec<_> = matching
            .iter()
            .copied()
            .filter(|check| {
                check.validate().is_ok()
                    && check.environment == environment
                    && check.candidate == candidate
                    && check.fingerprint == local.fingerprint
                    && !local.dirty
                    && check
                        .task
                        .as_ref()
                        .is_none_or(|task| repo.tasks.iter().any(|required| task.covers(required)))
            })
            .collect();
        // The latest matching attempt wins. A later failure cannot be hidden
        // by retaining an older pass; equal-timestamp conflicts also fail closed.
        let latest = current.iter().map(|check| check.completed_at_ms).max();
        let latest_checks: Vec<_> = current
            .iter()
            .filter(|check| Some(check.completed_at_ms) == latest)
            .collect();
        let pending = current.iter().any(|check| {
            check.completed_at_ms.is_none()
                || matches!(
                    check.status,
                    TaskValidationStatus::Running | TaskValidationStatus::NotRun
                )
        });
        let passed = !pending
            && !latest_checks.is_empty()
            && latest_checks.iter().all(|check| {
                check.status == TaskValidationStatus::Passed && check.completed_at_ms.is_some()
            });
        if passed {
            check_report.status = WorkspaceMissionCheckStatus::Passed;
            check_report.detail = None;
            check_report.receipt_id = latest_checks.first().map(|check| check.receipt_id.clone());
            report.covered_checks.push(required.clone());
        } else if !matching.is_empty() {
            check_report.status = if current.is_empty() {
                if matching.iter().all(|check| check.validate().is_err()) {
                    WorkspaceMissionCheckStatus::Blocked
                } else {
                    WorkspaceMissionCheckStatus::Stale
                }
            } else if current
                .iter()
                .all(|check| check.status == TaskValidationStatus::NotRun)
            {
                WorkspaceMissionCheckStatus::NotRun
            } else if !pending
                && latest_checks
                    .iter()
                    .any(|check| check.status == TaskValidationStatus::Failed)
            {
                WorkspaceMissionCheckStatus::Failed
            } else {
                WorkspaceMissionCheckStatus::Blocked
            };
            check_report.detail = Some(match check_report.status {
                WorkspaceMissionCheckStatus::Stale => "Receipt does not match the candidate, worktree fingerprint, environment or linked run",
                WorkspaceMissionCheckStatus::Failed => "The latest matching validation attempt failed",
                WorkspaceMissionCheckStatus::NotRun => "The linked validation has not run",
                _ => "Validation is running, incomplete, cancelled, inconclusive or has invalid provenance fields",
            }.into());
            report.errors.push(format!("Validation '{required}' is failed, pending, stale or from a different environment/task run"));
        } else if observed.error.is_some() {
            check_report.status = WorkspaceMissionCheckStatus::Blocked;
            check_report.detail = observed.error.clone();
        }
        // Retain the receipt that explains a non-success state as well. Never
        // link an arbitrary attempt when simultaneous receipts disagree. A
        // historical receipt is navigation context, not a current pass.
        if check_report.receipt_id.is_none() {
            let relevant: Vec<_> = matching
                .iter()
                .copied()
                .filter(|check| check.validate().is_ok())
                .filter(|check| match check_report.status {
                    WorkspaceMissionCheckStatus::Stale => true,
                    WorkspaceMissionCheckStatus::Failed => {
                        current.contains(check) && check.status == TaskValidationStatus::Failed
                    }
                    WorkspaceMissionCheckStatus::Blocked => {
                        current.contains(check)
                            && if pending {
                                check.completed_at_ms.is_none()
                                    || matches!(
                                        check.status,
                                        TaskValidationStatus::Running
                                            | TaskValidationStatus::NotRun
                                    )
                            } else {
                                check.status != TaskValidationStatus::Passed
                            }
                    }
                    WorkspaceMissionCheckStatus::NotRun => {
                        current.contains(check) && check.status == TaskValidationStatus::NotRun
                    }
                    WorkspaceMissionCheckStatus::Passed => false,
                })
                .collect();
            let latest = relevant.iter().map(|check| check.completed_at_ms).max();
            let receipts: BTreeSet<_> = relevant
                .iter()
                .filter(|check| Some(check.completed_at_ms) == latest)
                .map(|check| check.receipt_id.as_str())
                .collect();
            if receipts.len() == 1 {
                check_report.receipt_id = receipts.first().map(|receipt| (*receipt).into());
            }
        }
        report.checks.push(check_report);
    }
    report
        .missing_checks
        .retain(|check| !report.covered_checks.contains(check));
    report.validated = !repo.required_checks.is_empty() && report.missing_checks.is_empty();
    report
}

fn remote_matches(
    identity: &WorkspaceMissionRepositoryIdentity,
    remote: &WorkspaceMissionRemoteEvidence,
) -> bool {
    remote.repository.validate().is_ok()
        && identity.matches(&remote.repository)
        && valid_ref(&remote.ref_name).is_ok()
        && valid_commit(&remote.head).is_ok()
}

fn visible(value: &str, limit: usize, label: &str) -> Result<(), String> {
    if value.trim().is_empty()
        || value.len() > limit
        || value.chars().any(|ch| {
            (ch.is_control() && !matches!(ch, '\n' | '\t'))
                || (!ch.is_control() && crate::review_input::is_visual_spoofing_character(ch))
        })
    {
        return Err(format!("{label} must be bounded, nonempty visible text"));
    }
    Ok(())
}

fn valid_commit(commit: &str) -> Result<(), String> {
    if !matches!(commit.len(), 40 | 64)
        || !commit
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err("Use a full lowercase commit SHA, never a branch or abbreviated commit".into());
    }
    Ok(())
}

fn valid_ref(reference: &str) -> Result<(), String> {
    if !reference.starts_with("refs/heads/")
        || reference.len() > 512
        || reference.ends_with('/')
        || reference.contains("..")
        || reference.contains("@{")
        || reference.contains("//")
        || reference
            .split('/')
            .any(|part| part.starts_with('.') || part.ends_with('.') || part.ends_with(".lock"))
        || reference
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c))
    {
        return Err("Use an exact refs/heads/... reference".into());
    }
    Ok(())
}

fn valid_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || path.as_os_str().len() > 4096
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        || path.to_string_lossy().contains('\0')
    {
        return Err("Mission repository paths must be absolute without parent traversal".into());
    }
    Ok(())
}

fn normal_path(path: &Path) -> PathBuf {
    path.components().collect()
}

fn unique_checks(checks: &[String]) -> Result<(), String> {
    if checks.len() > WORKSPACE_MISSION_CHECKS {
        return Err("Too many required validation checks".into());
    }
    let mut seen = BTreeSet::new();
    for check in checks {
        visible(check, 128, "Validation check name")?;
        if !seen.insert(check) {
            return Err("Required validation check names must be unique".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(value: char) -> String {
        std::iter::repeat_n(value, 40).collect()
    }

    fn fixture(preset: bool) -> (WorkspaceMission, Vec<WorkspaceMissionRepositoryEvidence>) {
        let mut mission = if preset {
            WorkspaceMission::four_terminals(
                "Five repositories",
                "Pass every declared check",
                "/work",
            )
        } else {
            let mut mission = WorkspaceMission::new("One repository", "Pass the declared check");
            mission
                .repositories
                .push(WorkspaceMissionRepository::new("example/project", "/work/project").unwrap());
            mission
        };
        let evidence = mission
            .repositories
            .iter_mut()
            .enumerate()
            .map(|(index, repo)| {
                repo.baseline = sha('a');
                repo.candidate = Some(sha('b'));
                if preset && index > 0 {
                    repo.required_core_pin = Some(sha('b'));
                }
                let remote = WorkspaceMissionRemoteEvidence {
                    repository: repo.identity.clone(),
                    ref_name: repo.target_ref.clone(),
                    head: sha('b'),
                    observed_at_ms: 20,
                };
                WorkspaceMissionRepositoryEvidence {
                    identity: repo.identity.clone(),
                    path: repo.path.clone(),
                    local: Some(WorkspaceMissionLocalEvidence {
                        baseline: sha('a'),
                        head: sha('b'),
                        fingerprint: "fingerprint".into(),
                        modified: true,
                        dirty: false,
                        manifest_core_pin: repo.required_core_pin.clone(),
                        lock_core_pin: repo.required_core_pin.clone(),
                        observed_at_ms: 20,
                    }),
                    pushed: Some(remote.clone()),
                    integrated: Some(WorkspaceMissionIntegrationEvidence {
                        remote,
                        candidate: sha('b'),
                        contains_candidate: true,
                    }),
                    validations: vec![WorkspaceMissionValidationEvidence {
                        check_name: "validation".into(),
                        environment: "local".into(),
                        candidate: sha('b'),
                        fingerprint: "fingerprint".into(),
                        status: TaskValidationStatus::Passed,
                        task: None,
                        source: WorkspaceMissionValidationSource::Ci,
                        receipt_id: "host-ci-run:123".into(),
                        completed_at_ms: Some(10),
                    }],
                    error: None,
                }
            })
            .collect();
        (mission, evidence)
    }

    #[test]
    fn mission_attention_complete_and_uninspected_are_distinct() {
        use crate::agent_workspace::WorkspaceMissionAttentionKind as Kind;
        let (mission, evidence) = fixture(true);
        assert!(mission
            .attention_items(Some(&mission.project(&evidence)))
            .is_empty());
        let items = mission.attention_items(None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, Kind::Inspect);
        let empty = WorkspaceMission::new("Empty", "Declare input");
        assert_eq!(empty.attention_items(None)[0].kind, Kind::EditPlan);
    }

    #[test]
    fn mission_attention_saved_or_wrong_revision_requests_only_reinspection() {
        use crate::agent_workspace::WorkspaceMissionAttentionKind as Kind;
        let (mission, evidence) = fixture(true);
        let mut report = mission.project(&evidence);
        report.require_reinspection();
        let items = mission.attention_items(Some(&report));
        assert_eq!(
            items.len(),
            1,
            "do not suggest redundant validation or publication"
        );
        assert_eq!(items[0].kind, Kind::Inspect);
        report.revision += 1;
        assert_eq!(
            mission.attention_items(Some(&report))[0].kind,
            Kind::Inspect
        );
    }

    #[test]
    fn mission_attention_prerequisites_suppress_cascading_missing_gates() {
        use crate::agent_workspace::WorkspaceMissionAttentionKind as Kind;
        for (change, expected) in [
            Kind::RepositoryUnavailable,
            Kind::SelectCandidate,
            Kind::ReviewCandidate,
            Kind::DependencyPin,
        ]
        .into_iter()
        .enumerate()
        {
            let (mut mission, mut evidence) = fixture(true);
            match change {
                0 => evidence[1].local = None,
                1 => mission.repositories[1].candidate = None,
                2 => evidence[1].local.as_mut().unwrap().dirty = true,
                _ => evidence[1].local.as_mut().unwrap().lock_core_pin = None,
            }
            let items = mission.attention_items(Some(&mission.project(&evidence)));
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].kind, expected);
            assert_eq!(
                items[0].repository.as_ref().unwrap(),
                &mission.repositories[1].identity
            );
        }
    }

    #[test]
    fn mission_attention_order_and_ids_do_not_depend_on_repository_order() {
        use crate::agent_workspace::WorkspaceMissionAttentionKind as Kind;
        let (mut mission, mut evidence) = fixture(true);
        evidence[0].validations[0].status = TaskValidationStatus::Failed;
        evidence[1].validations.clear();
        evidence[2].pushed = None;
        let items = mission.attention_items(Some(&mission.project(&evidence)));
        assert_eq!(items[0].kind, Kind::ValidationFailed);
        assert_eq!(items[0].receipt_id.as_deref(), Some("host-ci-run:123"));
        mission.repositories.reverse();
        evidence.reverse();
        assert_eq!(
            items,
            mission.attention_items(Some(&mission.project(&evidence)))
        );
        mission.revision += 1;
        let revised = mission.attention_items(Some(&mission.project(&evidence)));
        assert_eq!(items[0].id, revised[0].id);
        assert_ne!(
            items[0], revised[0],
            "stable IDs alone cannot authorize stale callbacks"
        );
    }

    #[test]
    fn mission_attention_links_only_unambiguous_receipt_runs() {
        let (mut mission, mut evidence) = fixture(false);
        let task_id = TaskId::new();
        let linked = WorkspaceMissionTaskRef {
            conversation: ConversationId(format!(
                "jterm:task:v1:{}",
                serde_json::to_string(&("host", "namespace", task_id.to_string())).unwrap()
            )),
            task_id,
            run_id: "run-with:colon".into(),
            source_sequence: 7,
        };
        mission.repositories[0].tasks.push(linked.clone());
        let check = &mut evidence[0].validations[0];
        check.status = TaskValidationStatus::Failed;
        check.source = WorkspaceMissionValidationSource::Workspace;
        check.task = Some(linked.clone());
        check.receipt_id = "workspace:run-with:colon:7".into();
        let report = mission.project(&evidence);
        let items = mission.attention_items(Some(&report));
        assert_eq!(items[0].task.as_ref(), Some(&linked));
        assert_eq!(items[0].check_name.as_deref(), Some("validation"));
        for receipt in [
            "workspace:other-run:7",
            "workspace:run-with:colon:6",
            "workspace:run-with:colon:7:extra",
        ] {
            let mut report = report.clone();
            report.repositories[0].checks[0].receipt_id = Some(receipt.into());
            assert!(mission.attention_items(Some(&report))[0].task.is_none());
        }
        let mut ambiguous = linked;
        ambiguous.conversation.0 = ambiguous.conversation.0.replace("namespace", "other");
        mission.repositories[0].tasks.push(ambiguous);
        assert!(mission.attention_items(Some(&report))[0].task.is_none());
    }

    #[test]
    fn mission_check_failure_receipt_is_latest_and_ambiguity_has_no_shortcut() {
        let (mission, mut evidence) = fixture(false);
        let mut failure = evidence[0].validations[0].clone();
        failure.receipt_id = "failed-receipt".into();
        failure.status = TaskValidationStatus::Failed;
        failure.completed_at_ms = Some(11);
        evidence[0].validations.push(failure.clone());
        let report = mission.project(&evidence);
        assert_eq!(
            report.repositories[0].checks[0].receipt_id.as_deref(),
            Some("failed-receipt")
        );
        failure.receipt_id = "simultaneous-failure".into();
        evidence[0].validations.push(failure);
        assert!(mission.project(&evidence).repositories[0].checks[0]
            .receipt_id
            .is_none());
    }

    #[test]
    fn mission_pending_receipt_does_not_link_an_older_failed_attempt() {
        let (mission, mut evidence) = fixture(false);
        evidence[0].validations[0].status = TaskValidationStatus::Failed;
        let mut pending = evidence[0].validations[0].clone();
        pending.status = TaskValidationStatus::Running;
        pending.completed_at_ms = None;
        pending.receipt_id = "pending-receipt".into();
        evidence[0].validations.push(pending);
        let report = mission.project(&evidence);
        assert_eq!(
            report.repositories[0].checks[0].status,
            WorkspaceMissionCheckStatus::Blocked
        );
        assert_eq!(
            report.repositories[0].checks[0].receipt_id.as_deref(),
            Some("pending-receipt")
        );
    }

    #[test]
    fn mission_check_stale_receipt_remains_navigation_only() {
        use crate::agent_workspace::WorkspaceMissionAttentionKind as Kind;
        let (mission, mut evidence) = fixture(false);
        evidence[0].validations[0].fingerprint = "old-input".into();
        let report = mission.project(&evidence);
        assert!(!report.repositories[0].validated);
        let items = mission.attention_items(Some(&report));
        assert_eq!(items[0].kind, Kind::ValidationStale);
        assert_eq!(items[0].receipt_id.as_deref(), Some("host-ci-run:123"));
        assert!(items[0].task.is_none());
    }

    #[test]
    fn custom_bundle_and_five_repo_preset_both_complete() {
        for preset in [false, true] {
            let (mission, evidence) = fixture(preset);
            assert_eq!(
                mission.project(&evidence).state,
                WorkspaceMissionState::Completed
            );
        }
    }

    #[test]
    fn reloaded_observations_preserve_receipts_but_never_replay_current_success() {
        let (mission, evidence) = fixture(true);
        let mut report = mission.project(&evidence);
        assert_eq!(report.state, WorkspaceMissionState::Completed);
        report.require_reinspection();
        assert_eq!(report.state, WorkspaceMissionState::NeedsAttention);
        assert!(report.needs_you());
        for repository in &report.repositories {
            assert!(repository.observed.is_some());
            assert!(!repository.committed && !repository.pushed && !repository.integrated);
            assert!(!repository.validated && !repository.pins_current);
            assert_eq!(
                repository.pushed_observation.as_ref().unwrap().head,
                sha('b')
            );
            let integrated = repository.integration_observation.as_ref().unwrap();
            assert_eq!(integrated.candidate, sha('b'));
            assert_eq!(integrated.remote.ref_name, "refs/heads/master");
            assert_eq!(integrated.remote.observed_at_ms, 20);
            assert!(repository.covered_checks.is_empty());
            assert_eq!(repository.missing_checks, ["validation"]);
            assert_eq!(
                repository.checks[0].status,
                WorkspaceMissionCheckStatus::Stale
            );
            assert_eq!(
                repository.checks[0].receipt_id.as_deref(),
                Some("host-ci-run:123")
            );
        }
        let once = report.clone();
        report.require_reinspection();
        assert_eq!(report, once, "repeated refresh must not duplicate errors");
    }

    #[test]
    fn legacy_repository_reports_without_remote_receipts_still_decode() {
        let (mission, evidence) = fixture(false);
        let report = mission.project(&evidence);
        let mut value = serde_json::to_value(&report.repositories[0]).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("pushed_observation");
        object.remove("integration_observation");
        let legacy: WorkspaceMissionRepositoryReport = serde_json::from_value(value).unwrap();
        assert!(legacy.pushed_observation.is_none());
        assert!(legacy.integration_observation.is_none());
        assert_eq!(legacy.identity, report.repositories[0].identity);
    }

    #[test]
    fn missing_one_repository_never_completes_five_repo_bundle() {
        let (mut mission, mut evidence) = fixture(true);
        evidence.pop();
        assert_ne!(
            mission.project(&evidence).state,
            WorkspaceMissionState::Completed
        );
        mission.repositories.pop();
        assert!(mission.validate().is_err());
        assert_ne!(
            mission.project(&evidence).state,
            WorkspaceMissionState::Completed
        );
    }

    #[test]
    fn stages_do_not_imply_each_other() {
        let (mission, mut evidence) = fixture(false);
        evidence[0].pushed = None;
        evidence[0].integrated = None;
        evidence[0].validations.clear();
        let report = mission.project(&evidence);
        let repo = &report.repositories[0];
        assert!(repo.modified && repo.committed);
        assert!(!repo.pushed && !repo.integrated && !repo.validated);
        assert_ne!(report.state, WorkspaceMissionState::Completed);
    }

    #[test]
    fn partly_integrated_bundle_and_advanced_remote_head_remain_distinct() {
        let (mission, mut evidence) = fixture(true);
        evidence[4].integrated = None;
        assert_eq!(
            mission.project(&evidence).state,
            WorkspaceMissionState::PartiallyIntegrated
        );
        let (mission, mut evidence) = fixture(false);
        evidence[0].integrated.as_mut().unwrap().remote.head = sha('c');
        evidence[0].pushed = Some(evidence[0].integrated.as_ref().unwrap().remote.clone());
        assert_eq!(
            mission.project(&evidence).state,
            WorkspaceMissionState::Completed
        );
        evidence[0].integrated.as_mut().unwrap().contains_candidate = false;
        let report = mission.project(&evidence);
        assert!(!report.repositories[0].pushed && !report.repositories[0].integrated);
    }

    #[test]
    fn each_check_exposes_pass_fail_blocked_not_run_and_stale() {
        for expected in [
            WorkspaceMissionCheckStatus::Passed,
            WorkspaceMissionCheckStatus::Failed,
            WorkspaceMissionCheckStatus::Blocked,
            WorkspaceMissionCheckStatus::NotRun,
            WorkspaceMissionCheckStatus::Stale,
        ] {
            let (mission, mut evidence) = fixture(false);
            match expected {
                WorkspaceMissionCheckStatus::Passed => {}
                WorkspaceMissionCheckStatus::Failed => {
                    evidence[0].validations[0].status = TaskValidationStatus::Failed
                }
                WorkspaceMissionCheckStatus::Blocked => {
                    evidence[0].validations[0].status = TaskValidationStatus::Running;
                    evidence[0].validations[0].completed_at_ms = None;
                }
                WorkspaceMissionCheckStatus::NotRun => evidence[0].validations.clear(),
                WorkspaceMissionCheckStatus::Stale => {
                    evidence[0].validations[0].environment = "different-host".into()
                }
            }
            assert_eq!(
                mission.project(&evidence).repositories[0].checks[0].status,
                expected
            );
        }
    }

    #[test]
    fn wrong_candidate_dirty_tree_environment_and_fingerprint_are_stale() {
        for change in 0..4 {
            let (mission, mut evidence) = fixture(false);
            match change {
                0 => evidence[0].validations[0].candidate = sha('c'),
                1 => evidence[0].local.as_mut().unwrap().dirty = true,
                2 => evidence[0].validations[0].environment = "different-machine".into(),
                _ => evidence[0].validations[0].fingerprint = "old-worktree".into(),
            }
            let report = mission.project(&evidence);
            assert!(!report.repositories[0].validated);
            assert_ne!(report.state, WorkspaceMissionState::Completed);
        }
    }

    #[test]
    fn remote_repo_and_target_ref_must_match() {
        let (mission, mut evidence) = fixture(false);
        evidence[0].pushed.as_mut().unwrap().repository.full_name = "example/wrong".into();
        evidence[0].integrated.as_mut().unwrap().remote.ref_name = "refs/heads/feature".into();
        let report = mission.project(&evidence);
        assert!(!report.repositories[0].pushed && !report.repositories[0].integrated);
        assert_ne!(report.state, WorkspaceMissionState::Completed);
    }

    #[test]
    fn core_pin_and_lockfile_must_agree_with_core_candidate() {
        let (mut mission, mut evidence) = fixture(true);
        evidence[1].local.as_mut().unwrap().lock_core_pin = Some(sha('c'));
        assert!(!mission.project(&evidence).repositories[1].pins_current);
        evidence[1].local.as_mut().unwrap().lock_core_pin = Some(sha('b'));
        mission.repositories[0].candidate = Some(sha('c'));
        assert!(!mission.project(&evidence).repositories[1].pins_current);
    }

    #[test]
    fn latest_failure_and_ambiguous_evidence_cannot_reuse_old_pass() {
        let (mission, mut evidence) = fixture(false);
        let mut failure = evidence[0].validations[0].clone();
        failure.status = TaskValidationStatus::Failed;
        failure.completed_at_ms = Some(11);
        evidence[0].validations.push(failure);
        assert!(!mission.project(&evidence).repositories[0].validated);
        evidence[0].validations.pop();
        let mut pending = evidence[0].validations[0].clone();
        pending.status = TaskValidationStatus::Running;
        pending.completed_at_ms = None;
        evidence[0].validations.push(pending);
        assert!(!mission.project(&evidence).repositories[0].validated);
        evidence[0].validations.pop();
        evidence.push(evidence[0].clone());
        assert_ne!(
            mission.project(&evidence).state,
            WorkspaceMissionState::Completed
        );
    }

    #[test]
    fn empty_checks_and_missing_baseline_do_not_make_completion_vacuous() {
        let (mut mission, evidence) = fixture(false);
        mission.repositories[0].required_checks.clear();
        assert_ne!(
            mission.project(&evidence).state,
            WorkspaceMissionState::Completed
        );
        let (mut mission, evidence) = fixture(false);
        mission.repositories[0].baseline.clear();
        assert_ne!(
            mission.project(&evidence).state,
            WorkspaceMissionState::Completed
        );
        assert_eq!(
            WorkspaceMission::new("Draft", "An acceptance goal")
                .project(&[])
                .state,
            WorkspaceMissionState::Draft
        );
    }

    #[test]
    fn draft_can_be_saved_and_inspected_before_adopting_candidates() {
        let (mut mission, evidence) = fixture(false);
        mission.repositories[0].baseline.clear();
        mission.repositories[0].candidate = None;
        assert!(mission.validate().is_ok());
        let saved = mission
            .apply_update(
                &WorkspaceMissionUpdate {
                    expected_revision: 0,
                    mission: mission.clone(),
                },
                1,
            )
            .unwrap();
        let report = saved.project(&evidence);
        assert_ne!(report.state, WorkspaceMissionState::Completed);
        assert_eq!(
            report.repositories[0].observed.as_ref().unwrap().head,
            sha('b')
        );
    }

    #[test]
    fn all_declared_checks_need_current_coverage() {
        let (mut mission, evidence) = fixture(false);
        mission.repositories[0]
            .required_checks
            .push("native-smoke".into());
        let report = mission.project(&evidence);
        assert!(!report.repositories[0].validated);
        assert_eq!(report.repositories[0].covered_checks, vec!["validation"]);
        assert_eq!(report.repositories[0].missing_checks, vec!["native-smoke"]);
    }

    #[test]
    fn revisions_are_compared_and_invalid_updates_are_atomic() {
        let (mut mission, _) = fixture(false);
        mission.revision = 7;
        let mut update = WorkspaceMissionUpdate {
            expected_revision: 7,
            mission: mission.clone(),
        };
        update.mission.title = "Changed title".into();
        let next = mission.apply_update(&update, 99).unwrap();
        assert_eq!((next.revision, next.updated_at_ms), (8, 99));
        assert_eq!(mission.revision, 7);
        assert!(next.apply_update(&update, 100).is_err());
        update.mission.repositories[0].candidate = Some("master".into());
        assert!(mission.apply_update(&update, 100).is_err());
        assert_eq!(mission.title, "One repository");
        mission.revision = u64::MAX;
        assert!(mission
            .apply_update(
                &WorkspaceMissionUpdate {
                    expected_revision: u64::MAX,
                    mission: mission.clone(),
                },
                100
            )
            .is_err());
    }

    #[test]
    fn task_reference_is_exact_and_does_not_create_a_parallel_identity() {
        let task_id = TaskId::new();
        let mut task = WorkspaceMissionTaskRef {
            conversation: ConversationId(format!(
                "jterm:task:v1:{}",
                serde_json::to_string(&("host", "namespace", task_id.to_string())).unwrap()
            )),
            task_id,
            run_id: "existing-host-incarnation".into(),
            source_sequence: 4,
        };
        assert!(task.validate().is_ok());
        let mut newer = task.clone();
        newer.source_sequence = 5;
        assert!(newer.covers(&task));
        newer.run_id = "another-incarnation".into();
        assert!(!newer.covers(&task));
        task.task_id = TaskId::new();
        assert!(task.validate().is_err());
    }

    #[test]
    fn existing_result_binding_ignores_ui_status_text_and_rejects_stale_review() {
        use super::super::WorkspaceValidationEvidence;
        use crate::agent_task::worktree::WorktreeReviewSnapshot;

        let task_id = TaskId::new();
        let mut result = WorkspaceResultReport {
            conversation: ConversationId(format!(
                "jterm:task:v1:{}",
                serde_json::to_string(&("host", "namespace", task_id.to_string())).unwrap()
            )),
            task_id,
            run_id: "existing-incarnation".into(),
            source_sequence: 3,
            baseline: None,
            current: Some(WorktreeReviewSnapshot {
                head: sha('b'),
                branch: "feature".into(),
                base_commit: sha('a'),
                fingerprint: "worktree-fingerprint".into(),
                status: String::new(),
                diff: String::new(),
                diff_truncated: false,
                status_truncated: false,
                untracked_files: 0,
                captured_at_ms: 10,
                attribution: "host".into(),
            }),
            validation: Some(WorkspaceValidationEvidence {
                fingerprint: "worktree-fingerprint".into(),
                head: sha('b'),
                base_commit: sha('a'),
                command: "cargo test".into(),
                cwd: "/work/project".into(),
                shell: "/bin/sh".into(),
                started_at_ms: 5,
                completed_at_ms: Some(9),
                unchanged_after_run: true,
                detail: None,
            }),
            validation_current: true,
            validation_status: "Passed".into(),
            validation_output: "model says everything passed".into(),
            validation_output_truncated: false,
            error: None,
        };
        let receipt = WorkspaceMissionValidationEvidence::from_result(
            "validation",
            "local",
            &result,
            TaskValidationStatus::Failed,
        )
        .unwrap();
        assert_eq!(receipt.status, TaskValidationStatus::Failed);
        assert_eq!(receipt.task.as_ref().unwrap().task_id, task_id);
        let (mut mission, mut evidence) = fixture(false);
        mission.repositories[0]
            .tasks
            .push(receipt.task.clone().unwrap());
        evidence[0].local.as_mut().unwrap().fingerprint = receipt.fingerprint.clone();
        evidence[0].validations = vec![receipt];
        let item = mission
            .attention_items(Some(&mission.project(&evidence)))
            .remove(0);
        assert!(item.matches_result(&result));
        for change in 0..4 {
            let mut changed = result.clone();
            match change {
                0 => changed.source_sequence += 1,
                1 => changed.run_id = "another-run".into(),
                2 => changed.conversation.0 = "another-conversation".into(),
                _ => changed.task_id = TaskId::new(),
            }
            assert!(!item.matches_result(&changed));
        }
        result.current.as_mut().unwrap().fingerprint = "edited-after-validation".into();
        assert!(WorkspaceMissionValidationEvidence::from_result(
            "validation",
            "local",
            &result,
            TaskValidationStatus::Passed,
        )
        .is_err());
    }

    #[test]
    fn repository_id_mismatch_is_explicit_but_zero_is_unknown() {
        let mut identity = WorkspaceMissionRepositoryIdentity::canonical("beamiter/forge").unwrap();
        assert_eq!(identity.github_id, 1_170_023_936);
        identity.github_id = 1;
        assert!(identity.validate().unwrap_err().contains("mismatch"));
        identity.github_id = 0;
        assert!(identity.validate().is_ok());
    }

    #[test]
    fn model_text_is_not_an_evidence_source() {
        let (_, evidence) = fixture(false);
        let mut value = serde_json::to_value(&evidence[0].validations[0]).unwrap();
        value["source"] = serde_json::json!("gpt-remote-passed");
        assert!(serde_json::from_value::<WorkspaceMissionValidationEvidence>(value).is_err());
    }
}
