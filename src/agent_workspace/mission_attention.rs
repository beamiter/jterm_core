//! Read-only, stable Needs-you entries for native Mission surfaces.
//! These describe the next manual action. They grant no execution authority.
use super::*;
use serde::{Deserialize, Serialize};

/// Priority order is shared by all frontends, independent of catalog order.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMissionAttentionKind {
    ValidationFailed,
    ValidationBlocked,
    ValidationStale,
    Inspect,
    EditPlan,
    RepositoryUnavailable,
    SelectCandidate,
    ReviewCandidate,
    DependencyPin,
    ValidationNotRun,
    PushCandidate,
    IntegrateCandidate,
    ReviewEvidence,
}

impl WorkspaceMissionAttentionKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::ValidationFailed => "Validation failed",
            Self::ValidationBlocked => "Validation blocked",
            Self::ValidationStale => "Validation needs rechecking",
            Self::Inspect => "Inspect current delivery",
            Self::EditPlan => "Complete the acceptance plan",
            Self::RepositoryUnavailable => "Locate the repository",
            Self::SelectCandidate => "Select an exact candidate",
            Self::ReviewCandidate => "Review candidate changes",
            Self::DependencyPin => "Correct the core dependency pin",
            Self::ValidationNotRun => "Run the required validation",
            Self::PushCandidate => "Verify the pushed candidate",
            Self::IntegrateCandidate => "Verify target integration",
            Self::ReviewEvidence => "Review incomplete evidence",
        }
    }
}

/// An ephemeral projection, not a new task or persisted approval. `id` omits
/// timestamps and catalog indices so selection survives harmless refreshes.
/// Before acting on a callback, compare the full entry with the current queue:
/// a stable ID alone does not establish that its revision/receipt is current.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMissionAttention {
    pub id: String,
    pub mission_id: String,
    pub revision: u64,
    pub mission_title: String,
    pub repository: Option<WorkspaceMissionRepositoryIdentity>,
    pub candidate: Option<String>,
    pub kind: WorkspaceMissionAttentionKind,
    pub check_name: Option<String>,
    /// Required environment from the plan, not a claim about a stale receipt.
    pub environment: Option<String>,
    pub receipt_id: Option<String>,
    /// Only populated when the receipt unambiguously names a linked task run.
    /// A frontend must still verify that its current host exposes that run.
    pub task: Option<WorkspaceMissionTaskRef>,
    pub detail: String,
}

impl WorkspaceMissionAttention {
    /// Whether the existing result view is the exact receipt behind this item.
    /// Newer output or a reused TaskId cannot silently substitute another run.
    /// This binds the validation log identity, not mutable worktree contents:
    /// `report.current` is a separate capture and may differ from validation input.
    pub fn matches_result(&self, report: &WorkspaceResultReport) -> bool {
        self.task.as_ref().is_some_and(|task| {
            report.conversation == task.conversation
                && report.task_id == task.task_id
                && report.run_id == task.run_id
                && report.source_sequence >= task.source_sequence
                && self.receipt_id.as_deref()
                    == Some(
                        format!("workspace:{}:{}", report.run_id, report.source_sequence).as_str(),
                    )
        })
    }

    pub(crate) fn sort_key(&self) -> (WorkspaceMissionAttentionKind, &str) {
        (self.kind, &self.id)
    }
}

impl WorkspaceMission {
    /// Project actionable reasons from this exact saved revision. Missing,
    /// reopened or incompatible reports request inspection rather than turning
    /// every invalidated gate into an instruction to push or run work again.
    pub fn attention_items(
        &self,
        report: Option<&WorkspaceMissionReport>,
    ) -> Vec<WorkspaceMissionAttention> {
        use WorkspaceMissionAttentionKind as Kind;
        let mut items = Vec::new();
        if let Err(error) = self.validate() {
            items.push(self.attention(None, Kind::EditPlan, None, error));
            return items;
        }
        if self.repositories.is_empty() {
            items.push(self.attention(
                None,
                Kind::EditPlan,
                None,
                "Add the repositories and acceptance checks for this delivery".into(),
            ));
            return items;
        }
        let Some(report) = report.filter(|r| r.id == self.id && r.revision == self.revision) else {
            items.push(self.attention(
                None,
                Kind::Inspect,
                None,
                "No inspection is available for this saved plan revision".into(),
            ));
            return items;
        };
        if report.repositories.iter().any(|repository| {
            repository
                .errors
                .iter()
                .any(|error| error == mission::REINSPECTION_DETAIL)
        }) {
            items.push(self.attention(
                None,
                Kind::Inspect,
                None,
                mission::REINSPECTION_DETAIL.into(),
            ));
            return items;
        }
        if !report.errors.is_empty() {
            items.push(self.attention(None, Kind::ReviewEvidence, None, report.errors.join("\n")));
        }
        for repository in &self.repositories {
            let Some(observed) = report.repositories.iter().find(|observed| {
                observed.identity.matches(&repository.identity) && observed.path == repository.path
            }) else {
                items.push(self.attention(
                    Some(repository),
                    Kind::RepositoryUnavailable,
                    None,
                    "Inspect the exact checkout declared in this plan".into(),
                ));
                continue;
            };
            if observed.complete() {
                continue;
            }
            let before = items.len();
            let primary = if observed.observed.is_none() {
                Some((
                    Kind::RepositoryUnavailable,
                    "The host could not capture this repository",
                ))
            } else if repository.candidate.is_none() || repository.baseline.is_empty() {
                Some((
                    Kind::SelectCandidate,
                    "Inspect clean input, then explicitly select the candidate commits",
                ))
            } else if !observed.committed {
                Some((
                    Kind::ReviewCandidate,
                    "The observed checkout is dirty or no longer matches the selected candidate",
                ))
            } else if !observed.pins_current {
                Some((
                    Kind::DependencyPin,
                    "The manifest and lockfile must match this Mission's exact core candidate",
                ))
            } else if repository.required_checks.is_empty() {
                Some((Kind::EditPlan, "Declare at least one acceptance check"))
            } else {
                None
            };
            if let Some((kind, detail)) = primary {
                let detail = if observed.errors.is_empty() {
                    detail.into()
                } else {
                    observed.errors.join("\n")
                };
                items.push(self.attention(Some(repository), kind, None, detail));
                continue;
            }
            for check in &observed.checks {
                let kind = match check.status {
                    WorkspaceMissionCheckStatus::Passed => continue,
                    WorkspaceMissionCheckStatus::Failed => Kind::ValidationFailed,
                    WorkspaceMissionCheckStatus::Blocked => Kind::ValidationBlocked,
                    WorkspaceMissionCheckStatus::Stale => Kind::ValidationStale,
                    WorkspaceMissionCheckStatus::NotRun => Kind::ValidationNotRun,
                };
                items.push(self.attention(
                    Some(repository),
                    kind,
                    Some(check),
                    check.detail.clone().unwrap_or_else(|| kind.label().into()),
                ));
            }
            // Publication facts are separate from validation. Present them as
            // verification requests, never an automatic push/merge instruction.
            if !observed.pushed {
                items.push(self.attention(Some(repository), Kind::PushCandidate, None,
                    "No matching remote candidate receipt is available; inspect before taking publication action".into()));
            } else if !observed.integrated {
                items.push(self.attention(
                    Some(repository),
                    Kind::IntegrateCandidate,
                    None,
                    format!("The candidate is not verified on {}", repository.target_ref),
                ));
            }
            if items.len() == before {
                items.push(self.attention(
                    Some(repository),
                    Kind::ReviewEvidence,
                    None,
                    if observed.errors.is_empty() {
                        "The delivery evidence is incomplete; inspect again".into()
                    } else {
                        observed.errors.join("\n")
                    },
                ));
            }
        }
        items.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        items
    }

    fn attention(
        &self,
        repository: Option<&WorkspaceMissionRepository>,
        kind: WorkspaceMissionAttentionKind,
        check: Option<&WorkspaceMissionCheckReport>,
        detail: String,
    ) -> WorkspaceMissionAttention {
        let receipt_id = check.and_then(|check| check.receipt_id.clone());
        let task = receipt_id.as_ref().and_then(|receipt| {
            let candidates: Vec<_> = repository?
                .tasks
                .iter()
                .filter(|task| {
                    task.validate().is_ok()
                        && receipt
                            .strip_prefix(&format!("workspace:{}:", task.run_id))
                            .and_then(|sequence| sequence.parse::<u64>().ok())
                            .is_some_and(|sequence| sequence >= task.source_sequence)
                })
                .collect();
            (candidates.len() == 1).then(|| candidates[0].clone())
        });
        WorkspaceMissionAttention {
            id: serde_json::to_string(&(
                &self.id,
                repository.map(|r| r.identity.full_name.to_ascii_lowercase()),
                kind,
                check.map(|c| &c.check_name),
            ))
            .expect("serializable attention key"),
            mission_id: self.id.clone(),
            revision: self.revision,
            mission_title: self.title.clone(),
            repository: repository.map(|r| r.identity.clone()),
            candidate: repository.and_then(|r| r.candidate.clone()),
            kind,
            check_name: check.map(|c| c.check_name.clone()),
            environment: check.map(|c| c.environment.clone()),
            receipt_id,
            task,
            detail,
        }
    }
}
