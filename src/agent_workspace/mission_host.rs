//! Worker-side facts. Neither a plan nor provider output can supply receipts.
use super::*;
use crate::agent_task::worktree::{capture_repository_review, inspect_repository_delivery};
use std::time::{Duration, Instant};

pub(super) fn inspect(
    mission: &WorkspaceMission,
    snapshots: &[NativeSnapshot],
) -> WorkspaceMissionReport {
    let started = Instant::now();
    let mut evidence = Vec::new();
    for repository in &mission.repositories {
        let mut observed = WorkspaceMissionRepositoryEvidence {
            identity: repository.identity.clone(),
            path: repository.path.clone(),
            local: None,
            pushed: None,
            integrated: None,
            validations: Vec::new(),
            error: None,
        };
        let result = (|| -> Result<(), String> {
            if started.elapsed() > Duration::from_secs(45) {
                return Err("Inspection budget reached; this repository was not checked".into());
            }
            let current = capture_repository_review(&repository.path, &repository.baseline)?;
            let delivery = inspect_repository_delivery(
                &repository.path,
                &repository.identity.full_name,
                &current.head,
                &repository.target_ref,
            );
            let (manifest_core_pin, lock_core_pin) = match &delivery {
                Ok(delivery) => dependency_pins(&delivery.manifest, &delivery.lockfile)?,
                Err(_) => (None, None),
            };
            // Pin inspection and remote reads may outlive external writes.
            // Never combine an earlier worktree with later dependency facts.
            let after = capture_repository_review(&repository.path, &repository.baseline)?;
            if current.fingerprint != after.fingerprint {
                return Err("Repository changed during Mission inspection; stop writers and inspect again".into());
            }
            observed.local = Some(WorkspaceMissionLocalEvidence {
                baseline: current.base_commit.clone(),
                head: current.head.clone(),
                fingerprint: current.fingerprint.clone(),
                modified: current.head != current.base_commit || !current.status.is_empty(),
                dirty: !current.status.is_empty(),
                manifest_core_pin,
                lock_core_pin,
                observed_at_ms: current.captured_at_ms,
            });
            match delivery {
                Ok(delivery) => {
                    if let Some(head) = delivery.remote_head {
                        let remote = WorkspaceMissionRemoteEvidence {
                            repository: repository.identity.clone(),
                            ref_name: repository.target_ref.clone(),
                            head,
                            observed_at_ms: super::model::now_ms(),
                        };
                        if remote.head == current.head || delivery.target_contains_candidate {
                            observed.pushed = Some(remote.clone());
                        }
                        observed.integrated = Some(WorkspaceMissionIntegrationEvidence {
                            remote,
                            candidate: current.head.clone(),
                            contains_candidate: delivery.target_contains_candidate,
                        });
                    }
                    observed.error = delivery.remote_error;
                }
                Err(error) => observed.error = Some(error),
            }
            for task in &repository.tasks {
                let Some(snapshot) = snapshots.iter().find(|snapshot| {
                    snapshot.id == task.conversation
                        && snapshot.task.id == task.task_id
                        && snapshot.incarnation == task.run_id
                }) else {
                    observed.error = Some("A linked task run is unavailable or changed".into());
                    continue;
                };
                if snapshot.task.worktree_path != repository.path {
                    observed.error = Some("Linked task belongs to a different worktree; select that checkout to bind its validation".into());
                    continue;
                }
                let report = WorkspaceResultReport::inspect_with_current(snapshot, Ok(current.clone()));
                if report.validation.as_ref().is_some_and(|validation| {
                    validation.started_at_ms <= mission.evidence_after_ms
                }) {
                    observed.error = Some("Validation predates this acceptance contract; validate again after saving the plan and candidates".into());
                    continue;
                }
                // Environment labels cannot be supplied by the saved plan to
                // upgrade a local run into GPU, Wayland, SSH or CI evidence.
                match WorkspaceMissionValidationEvidence::from_result(
                    "validation",
                    "local",
                    &report,
                    snapshot.task.validation.status,
                ) {
                    Ok(validation) => observed.validations.push(validation),
                    Err(error) => {
                        if snapshot.validation.is_some() {
                            observed.error = Some(error);
                        }
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            observed.error = Some(crate::redact::redact_secrets(&error));
        }
        evidence.push(observed);
    }
    mission.project(&evidence)
}

fn dependency_pins(manifest: &str, lockfile: &str) -> Result<(Option<String>, Option<String>), String> {
    let manifest: toml::Value = toml::from_str(manifest)
        .map_err(|error| format!("Candidate Cargo.toml cannot be inspected: {error}"))?;
    let lockfile: toml::Value = toml::from_str(lockfile)
        .map_err(|error| format!("Candidate Cargo.lock cannot be inspected: {error}"))?;
    let dependency = manifest.get("dependencies")
        .and_then(|dependencies| dependencies.get("jterm_core"));
    let source = dependency.and_then(|dependency| dependency.get("git"))
        .and_then(toml::Value::as_str);
    let expected_source = source.is_some_and(|source| matches!(source,
        "https://github.com/beamiter/jterm_core" | "https://github.com/beamiter/jterm_core.git"));
    let manifest_pin = dependency.filter(|_| expected_source)
        .and_then(|dependency| dependency.get("rev"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned);
    let packages = lockfile.get("package").and_then(toml::Value::as_array);
    let pins: Vec<_> = packages.into_iter().flatten()
        .filter(|package| package.get("name").and_then(toml::Value::as_str) == Some("jterm_core"))
        .filter_map(|package| package.get("source").and_then(toml::Value::as_str))
        .filter(|source| source.strip_prefix("git+").and_then(|source| source.split_once('?'))
            .is_some_and(|(repository, _)| matches!(repository,
                "https://github.com/beamiter/jterm_core" | "https://github.com/beamiter/jterm_core.git")))
        .filter_map(|source| source.rsplit_once('#').map(|(_, sha)| sha.to_owned()))
        .collect();
    if pins.len() > 1 {
        return Err("The candidate has multiple core dependency sources".into());
    }
    Ok((manifest_pin, pins.into_iter().next()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_and_lock_are_observed_independently() {
        let manifest = "[dependencies.jterm_core]\ngit='https://github.com/beamiter/jterm_core'\nrev='aaa'";
        let lockfile = "[[package]]\nname='jterm_core'\nsource='git+https://github.com/beamiter/jterm_core?rev=bbb#bbb'";
        assert_eq!(dependency_pins(manifest, lockfile).unwrap(), (Some("aaa".into()), Some("bbb".into())));
        assert!(dependency_pins("invalid toml [", lockfile).is_err());
    }
}
