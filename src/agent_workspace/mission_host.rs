//! Worker-side facts. Neither a plan nor provider output can supply receipts.
use super::*;
use crate::agent_task::worktree::{capture_repository_review, inspect_repository_delivery};
use std::collections::HashSet;
use std::time::{Duration, Instant};

pub(super) fn inspect(
    mission: &WorkspaceMission,
    snapshots: &[NativeSnapshot],
) -> WorkspaceMissionReport {
    let started = Instant::now();
    let mut evidence = Vec::new();
    let mut stale_checks = HashSet::new();
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
                Ok(delivery) => match dependency_pins(
                    &delivery.manifest,
                    &delivery.lockfile,
                    &delivery.cargo_configs,
                ) {
                    Ok(pins) => pins,
                    Err(error) => {
                        observed.error = Some(error);
                        (None, None)
                    }
                },
                Err(_) => (None, None),
            };
            // Pin inspection and remote reads may outlive external writes.
            // Never combine an earlier worktree with later dependency facts.
            let after = capture_repository_review(&repository.path, &repository.baseline)?;
            if current.fingerprint != after.fingerprint {
                return Err(
                    "Repository changed during Mission inspection; stop writers and inspect again"
                        .into(),
                );
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
                    if let Some(error) = delivery.remote_error {
                        observed.error.get_or_insert(error);
                    }
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
                let report =
                    WorkspaceResultReport::inspect_with_current(snapshot, Ok(current.clone()));
                if report
                    .validation
                    .as_ref()
                    .is_some_and(|validation| validation.started_at_ms <= mission.evidence_after_ms)
                {
                    stale_checks.insert(repository.identity.full_name.clone());
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
                        if report.validation.is_some() && !report.validation_current {
                            stale_checks.insert(repository.identity.full_name.clone());
                        }
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
    let mut report = mission.project(&evidence);
    for repository in &mut report.repositories {
        if stale_checks.contains(&repository.identity.full_name) {
            for check in &mut repository.checks {
                if check.check_name == "validation" {
                    check.status = WorkspaceMissionCheckStatus::Stale;
                    check.detail = Some("The recorded validation does not cover this acceptance contract and current worktree".into());
                }
            }
            // The stale label and aggregate gate must agree. In particular an
            // older linked run cannot leave the repository labelled validated.
            repository.validated = false;
            repository
                .covered_checks
                .retain(|name| name != "validation");
            if repository
                .checks
                .iter()
                .any(|check| check.check_name == "validation")
                && !repository
                    .missing_checks
                    .iter()
                    .any(|name| name == "validation")
            {
                repository.missing_checks.push("validation".into());
            }
        }
    }
    report
}

fn dependency_pins(
    manifest: &str,
    lockfile: &str,
    configs: &[(String, String)],
) -> Result<(Option<String>, Option<String>), String> {
    let manifest: toml::Value = toml::from_str(manifest)
        .map_err(|error| format!("Candidate Cargo.toml cannot be inspected: {error}"))?;
    let lockfile: toml::Value = toml::from_str(lockfile)
        .map_err(|error| format!("Candidate Cargo.lock cannot be inspected: {error}"))?;
    reject_source_overrides(&manifest, "Cargo.toml")?;
    for (name, content) in configs {
        let config: toml::Value = toml::from_str(content)
            .map_err(|error| format!("Candidate {name} cannot be inspected: {error}"))?;
        reject_source_overrides(&config, name)?;
    }
    let dependency = manifest
        .get("dependencies")
        .and_then(|dependencies| dependencies.get("jterm_core"));
    let source = dependency
        .and_then(|dependency| dependency.get("git"))
        .and_then(toml::Value::as_str);
    let expected_source = source.is_some_and(|source| {
        matches!(
            source,
            "https://github.com/beamiter/jterm_core" | "https://github.com/beamiter/jterm_core.git"
        )
    });
    let manifest_pin = dependency
        .filter(|_| expected_source)
        .and_then(|dependency| dependency.get("rev"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned);
    let packages = lockfile.get("package").and_then(toml::Value::as_array);
    let core_packages: Vec<_> = packages
        .into_iter()
        .flatten()
        .filter(|package| package.get("name").and_then(toml::Value::as_str) == Some("jterm_core"))
        .collect();
    if core_packages.len() > 1 {
        return Err("The candidate has multiple core dependency sources".into());
    }
    let pins: Vec<_> = core_packages
        .into_iter()
        .filter_map(|package| package.get("source").and_then(toml::Value::as_str))
        .filter(|source| {
            source
                .strip_prefix("git+")
                .and_then(|source| source.split_once('?'))
                .is_some_and(|(repository, _)| {
                    matches!(
                        repository,
                        "https://github.com/beamiter/jterm_core"
                            | "https://github.com/beamiter/jterm_core.git"
                    )
                })
        })
        .filter_map(|source| source.rsplit_once('#').map(|(_, sha)| sha.to_owned()))
        .collect();
    Ok((manifest_pin, pins.into_iter().next()))
}

fn reject_source_overrides(document: &toml::Value, name: &str) -> Result<(), String> {
    let patches_core = document
        .get("patch")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|sources| sources.values())
        .filter_map(toml::Value::as_table)
        .flat_map(|packages| packages.iter())
        .any(|(name, dependency)| {
            name == "jterm_core"
                || dependency.get("package").and_then(toml::Value::as_str) == Some("jterm_core")
        });
    let replaces_core = document
        .get("replace")
        .and_then(toml::Value::as_table)
        .is_some_and(|packages| packages.keys().any(|name| name.starts_with("jterm_core:")));
    let path_override = document
        .get("paths")
        .and_then(toml::Value::as_array)
        .is_some_and(|paths| !paths.is_empty());
    // Cargo source replacement needs independent content verification. A Git
    // SHA in Cargo.lock alone cannot establish what the replacement compiled.
    let source_replacement = document
        .get("source")
        .and_then(toml::Value::as_table)
        .is_some_and(|sources| {
            sources
                .values()
                .any(|source| source.get("replace-with").is_some())
        });
    if patches_core || replaces_core || path_override || source_replacement {
        return Err(format!(
            "Candidate {name} overrides dependency sources; its nominal Git pin does not prove the compiled core"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_and_lock_are_observed_independently() {
        let manifest =
            "[dependencies.jterm_core]\ngit='https://github.com/beamiter/jterm_core'\nrev='aaa'";
        let lockfile = "[[package]]\nname='jterm_core'\nsource='git+https://github.com/beamiter/jterm_core?rev=bbb#bbb'";
        assert_eq!(
            dependency_pins(manifest, lockfile, &[]).unwrap(),
            (Some("aaa".into()), Some("bbb".into()))
        );
        assert!(dependency_pins("invalid toml [", lockfile, &[]).is_err());
    }

    #[test]
    fn nominal_git_pins_cannot_hide_candidate_source_overrides() {
        let manifest =
            "[dependencies.jterm_core]\ngit='https://github.com/beamiter/jterm_core'\nrev='aaa'";
        let lockfile = "[[package]]\nname='jterm_core'\nsource='git+https://github.com/beamiter/jterm_core?rev=aaa#aaa'";
        for config in [
            "[patch.'https://github.com/beamiter/jterm_core.git']\njterm_core={path='../different-core'}",
            "[patch.'https://github.com/beamiter/jterm_core']\nrenamed={package='jterm_core',path='../different-core'}",
            "[replace]\n'jterm_core:0.2.0'={path='../different-core'}",
            "paths=['../different-core']",
            "[source.original]\nreplace-with='other'",
        ] {
            let configs = vec![(".cargo/config.toml".into(), config.into())];
            assert!(dependency_pins(manifest, lockfile, &configs)
                .unwrap_err()
                .contains("overrides dependency sources"));
        }
        let patched = format!(
            "{manifest}\n[patch.'https://github.com/beamiter/jterm_core']\njterm_core={{path='../different-core'}}"
        );
        assert!(dependency_pins(&patched, lockfile, &[]).is_err());
        let harmless = vec![(
            ".cargo/config.toml".into(),
            "[build]\njobs=2\n[patch.crates-io]\nunrelated={path='../unrelated'}".into(),
        )];
        assert_eq!(
            dependency_pins(manifest, lockfile, &harmless).unwrap(),
            (Some("aaa".into()), Some("aaa".into()))
        );
    }

    #[test]
    fn duplicate_path_or_other_repository_core_cannot_hide_behind_one_git_pin() {
        let manifest =
            "[dependencies.jterm_core]\ngit='https://github.com/beamiter/jterm_core'\nrev='aaa'";
        let lockfile = "[[package]]\nname='jterm_core'\nsource='git+https://github.com/beamiter/jterm_core?rev=aaa#aaa'\n[[package]]\nname='jterm_core'";
        assert!(dependency_pins(manifest, lockfile, &[])
            .unwrap_err()
            .contains("multiple core dependency sources"));
    }
}
