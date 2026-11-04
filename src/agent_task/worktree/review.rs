//! Read-only review evidence, bounded and tied to the verified task worktree.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const UNTRACKED_BYTES: usize = 16 * 1024 * 1024;
const UNTRACKED_FILES: usize = 2048;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorktreeReviewSnapshot {
    pub head: String,
    pub branch: String,
    pub base_commit: String,
    pub fingerprint: String,
    pub status: String,
    pub diff: String,
    #[serde(default)]
    pub diff_truncated: bool,
    #[serde(default)]
    pub status_truncated: bool,
    pub untracked_files: usize,
    pub captured_at_ms: u64,
    /// Includes all changes from the task baseline. This does not claim that
    /// every edit was made by the Agent; external writers are not attributed.
    pub attribution: String,
}

impl WorktreeService {
    pub fn capture_task_review(
        &self,
        task: &crate::agent_task::AgentTask,
    ) -> Result<WorktreeReviewSnapshot, WorktreeError> {
        let root = crate::agent_task::pinned_dir::PinnedDirectory::open(&task.worktree_path)
            .map_err(|error| WorktreeError::UnsafePath(error.to_string()))?;
        let path = root.proc_path();
        self.verify_active_task_worktree_through(
            &task.repo_root,
            &task.worktree_path,
            &path,
            &task.branch,
        )?;
        self.reject_external_checkout_filters(&task.repo_root)?;
        self.capture_review(&root, &task.base_commit, &task.branch)
    }

    fn capture_review(
        &self,
        root: &crate::agent_task::pinned_dir::PinnedDirectory,
        baseline: &str,
        branch: &str,
    ) -> Result<WorktreeReviewSnapshot, WorktreeError> {
        let path = root.proc_path();
        let head = self.resolve_head(&path)?;
        let baseline = self.resolve_commit(&path, baseline)?;
        let capture = |args: &[&str], operation: &str| -> Result<Vec<u8>, WorktreeError> {
            let mut command = base_git_command(&self.git, &path);
            command.args(args);
            let output = self.require_success(
                operation,
                self.run(command, operation, MAX_GIT_STDOUT_BYTES)?,
            )?;
            if output.stdout.truncated {
                return Err(WorktreeError::InvalidInput(format!(
                    "{operation} exceeds the 1 MiB evidence budget; review it externally"
                )));
            }
            Ok(output.stdout.bytes)
        };
        let status = capture(
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ],
            "review status",
        )?;
        let index = capture(&["ls-files", "--stage", "-z"], "review index identity")?;
        if index
            .split(|byte| *byte == 0)
            .any(|entry| entry.starts_with(b"160000 "))
        {
            return Err(WorktreeError::InvalidInput("Submodule contents need their own review evidence; automatic validation binding is unavailable for this worktree".into()));
        }
        let tracked = capture(
            &[
                "diff",
                "--binary",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=none",
                "HEAD",
                "--",
            ],
            "review worktree changes",
        )?;
        let staged = capture(
            &[
                "diff",
                "--cached",
                "--binary",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=none",
                "HEAD",
                "--",
            ],
            "review staged changes",
        )?;
        let diff = capture(
            &[
                "diff",
                "--binary",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=none",
                &baseline,
                "--",
            ],
            "review baseline changes",
        )?;
        let untracked = capture(
            &["ls-files", "--others", "--exclude-standard", "-z"],
            "review untracked paths",
        )?;
        let mut hash = Sha256::new();
        for bytes in [
            head.as_bytes(),
            baseline.as_bytes(),
            branch.as_bytes(),
            status.as_slice(),
            tracked.as_slice(),
            staged.as_slice(),
        ] {
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        let paths: Vec<_> = untracked
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
            .collect();
        if paths.len() > UNTRACKED_FILES {
            return Err(WorktreeError::InvalidInput(
                "Too many untracked files to bind validation evidence".into(),
            ));
        }
        let mut remaining = UNTRACKED_BYTES;
        for name in &paths {
            #[cfg(unix)]
            use std::os::unix::ffi::OsStrExt;
            let relative = Path::new(OsStr::from_bytes(name));
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_)))
            {
                return Err(WorktreeError::UnsafePath(
                    "Unexpected untracked path".into(),
                ));
            }
            let parent = relative.parent().unwrap_or(Path::new(""));
            let directory = root
                .open_beneath(parent)
                .map_err(|error| WorktreeError::UnsafePath(error.to_string()))?;
            let file_path =
                directory
                    .proc_path()
                    .join(relative.file_name().ok_or_else(|| {
                        WorktreeError::UnsafePath("Missing untracked filename".into())
                    })?);
            let metadata = fs::symlink_metadata(&file_path)
                .map_err(|error| WorktreeError::UnsafePath(error.to_string()))?;
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name);
            if metadata.file_type().is_symlink() {
                let target = fs::read_link(&file_path)
                    .map_err(|error| WorktreeError::UnsafePath(error.to_string()))?;
                hash.update(b"symlink");
                hash.update(target.as_os_str().as_bytes());
            } else if metadata.is_file() {
                use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
                let mut file = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(&file_path)
                    .map_err(|error| WorktreeError::UnsafePath(error.to_string()))?;
                let opened = file
                    .metadata()
                    .map_err(|error| WorktreeError::UnsafePath(error.to_string()))?;
                if !opened.is_file()
                    || metadata.ino() != opened.ino()
                    || metadata.dev() != opened.dev()
                    || opened.len() > remaining as u64
                {
                    return Err(WorktreeError::InvalidInput(
                        "Untracked material changed or exceeds the 16 MiB evidence budget".into(),
                    ));
                }
                let mut bytes = Vec::new();
                (&mut file)
                    .take(remaining as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|error| WorktreeError::UnsafePath(error.to_string()))?;
                let after = file
                    .metadata()
                    .map_err(|error| WorktreeError::UnsafePath(error.to_string()))?;
                if opened.len() != after.len()
                    || opened.mtime() != after.mtime()
                    || opened.mtime_nsec() != after.mtime_nsec()
                    || opened.ctime() != after.ctime()
                    || opened.ctime_nsec() != after.ctime_nsec()
                {
                    return Err(WorktreeError::InvalidInput(
                        "Untracked material changed during evidence capture".into(),
                    ));
                }
                if bytes.len() > remaining {
                    return Err(WorktreeError::InvalidInput(
                        "Untracked evidence exceeds its budget".into(),
                    ));
                }
                remaining -= bytes.len();
                hash.update(b"file");
                hash.update(opened.mode().to_le_bytes());
                hash.update((bytes.len() as u64).to_le_bytes());
                hash.update(&bytes);
            } else {
                return Err(WorktreeError::UnsafePath(
                    "Untracked evidence contains a special file or nested repository".into(),
                ));
            }
        }
        // Detect metadata or tracked-content movement during capture. The
        // evidence is still a point-in-time observation, not a filesystem lock.
        if head != self.resolve_head(&path)?
            || branch != self.git_text(
                git_command(&self.git, &path, ["rev-parse", "--abbrev-ref", "HEAD"]),
                "review branch recheck",
            )?
            || index != capture(&["ls-files", "--stage", "-z"], "review index recheck")?
            || staged != capture(
                &["diff", "--cached", "--binary", "--no-ext-diff", "--no-textconv", "--ignore-submodules=none", "HEAD", "--"],
                "review staged recheck",
            )?
            || status
                != capture(
                    &[
                        "status",
                        "--porcelain=v1",
                        "-z",
                        "--untracked-files=all",
                        "--ignore-submodules=none",
                    ],
                    "review status recheck",
                )?
            || tracked
                != capture(
                    &[
                        "diff",
                        "--binary",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--ignore-submodules=none",
                        "HEAD",
                        "--",
                    ],
                    "review content recheck",
                )?
        {
            return Err(WorktreeError::InvalidInput(
                "Worktree changed during evidence capture; refresh after writers stop".into(),
            ));
        }
        let safe = |bytes: &[u8]| {
            crate::review_input::safe_multiline_display(
                &crate::redact::redact_secrets(&String::from_utf8_lossy(bytes)),
                MAX_GIT_STDOUT_BYTES * 3,
            )
        };
        let status_text = safe(
            &status
                .iter()
                .map(|byte| if *byte == 0 { b'\n' } else { *byte })
                .collect::<Vec<_>>(),
        );
        let diff_text = safe(&diff);
        let status_truncated = status_text.len() > 16 * 1024;
        let diff_truncated = diff_text.len() > 128 * 1024;
        let mut attribution = "All changes from the task baseline; pre-existing and external edits are not attributed to an Agent. Ignored build output is excluded.".to_string();
        if status_truncated || diff_truncated {
            attribution.push_str(" Display is a bounded excerpt; the fingerprint covers the complete captured evidence.");
        }
        Ok(WorktreeReviewSnapshot {
            head,
            branch: branch.to_owned(),
            base_commit: baseline,
            fingerprint: format!("sha256:{:x}", hash.finalize()),
            status: crate::review_input::safe_multiline_display(&status_text, 16 * 1024),
            diff: crate::review_input::safe_multiline_display(&diff_text, 128 * 1024),
            diff_truncated,
            status_truncated,
            untracked_files: paths.len(),
            captured_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
            attribution,
        })
    }
}

/// Inspect an explicitly selected checkout using the same bounded evidence
/// capture as task worktrees. No managed directory or branch is created.
pub fn capture_repository_review(
    repository: &Path,
    baseline: &str,
) -> Result<WorktreeReviewSnapshot, String> {
    let service = super::delivery::read_only_service()?;
    let root = super::delivery::pin_repository(&service, repository)?;
    let path = root.proc_path();
    let branch = service
        .git_text(
            git_command(&service.git, &path, ["rev-parse", "--abbrev-ref", "HEAD"]),
            "repository branch",
        )
        .map_err(|error| error.to_string())?;
    let baseline = if baseline.is_empty() { "HEAD" } else { baseline };
    service
        .capture_review(&root, baseline, &branch)
        .map_err(|error| error.to_string())
}

pub fn capture_task_review(
    task: &crate::agent_task::AgentTask,
) -> Result<WorktreeReviewSnapshot, String> {
    let parent = task
        .worktree_path
        .parent()
        .ok_or("Task worktree has no managed parent")?;
    let mut service = WorktreeService::new(parent).map_err(|error| error.to_string())?;
    service.timeout = Duration::from_secs(5);
    service
        .capture_task_review(task)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn git(path: &Path, args: &[&str]) {
        let status = Command::new("/usr/bin/git")
            .args(args)
            .current_dir(path)
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "fixture Git failed: {args:?}");
    }
    #[test]
    fn evidence_changes_for_tracked_untracked_and_staged_content() {
        let root = std::env::temp_dir().join(format!("worktree-review-{}", uuid::Uuid::new_v4()));
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-b", "master"]);
        fs::write(repo.join("readme.txt"), "before\n").unwrap();
        git(&repo, &["add", "readme.txt"]);
        git(&repo, &["commit", "-m", "fixture"]);
        let service = WorktreeService::new(root.join("worktrees")).unwrap();
        let worktree = service
            .create(&CreateWorktreeRequest::new(
                &repo,
                "review",
                "review-fixture",
                "HEAD",
            ))
            .unwrap();
        let mut manager = crate::agent_task::TaskManager::new();
        let id = manager
            .create(crate::agent_task::NewTask {
                title: "Review".into(),
                provider: crate::agent_task::AgentProvider::Codex,
                repo_root: repo.clone(),
                worktree_path: worktree.path.clone(),
                branch: worktree.branch,
                base_commit: worktree.head,
                source_context: None,
            })
            .unwrap();
        let task = manager.get(id).unwrap();
        let first = capture_task_review(task).unwrap();
        fs::write(worktree.path.join("readme.txt"), "after\n").unwrap();
        let changed = capture_task_review(task).unwrap();
        assert_ne!(first.fingerprint, changed.fingerprint);
        assert!(changed.diff.contains("+after"));
        fs::write(worktree.path.join("notes.txt"), "first note").unwrap();
        let untracked = capture_task_review(task).unwrap();
        assert_eq!(untracked.untracked_files, 1);
        assert_ne!(changed.fingerprint, untracked.fingerprint);
        fs::write(worktree.path.join("notes.txt"), "other note").unwrap();
        assert_ne!(
            untracked.fingerprint,
            capture_task_review(task).unwrap().fingerprint
        );
        git(&worktree.path, &["add", "readme.txt"]);
        assert_ne!(
            changed.fingerprint,
            capture_task_review(task).unwrap().fingerprint
        );
        let stable = capture_task_review(task).unwrap();
        assert_eq!(
            stable.fingerprint,
            capture_task_review(task).unwrap().fingerprint
        );
        drop(service);
        let _ = fs::remove_dir_all(root);
    }
}
