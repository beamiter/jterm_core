//! Versioned, private checkpoints for explicitly authored tasks.
//!
//! This store is deliberately separate from creation journals and provider
//! transcripts. Opening, listing and checkpointing never starts a provider.
//! Hold the store lease while a recovered task is owned by a runtime.

use super::{
    AgentTask, NewTask, TaskBrief, TaskId, TaskManager, TaskRuntimeKind, TaskStatus,
    TaskValidationState, TaskValidationStatus, WorktreeService,
};
use crate::snapshot_file;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

const VERSION: u32 = 1;
pub const MAX_RECOVERY_TASKS: usize = 128;
const MAX_BYTES: u64 = 4 * 1024 * 1024;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Historical evidence only. Never copied into the manager's validation gate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationReceipt {
    pub attempt: u64,
    pub status: TaskValidationStatus,
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

impl DirectoryIdentity {
    fn read(path: &Path) -> io::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_dir() {
            return Err(invalid("recovery worktree is not a directory"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "recovery requires Unix",
        ))
    }
}

/// Allowlisted task data: no environment, command output, approvals, process or
/// terminal identities, captured context, provider session or error payloads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCheckpoint {
    pub id: TaskId,
    pub title: String,
    pub provider: super::AgentProvider,
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    pub base_commit: String,
    pub brief: TaskBrief,
    pub recorded_status: TaskStatus,
    pub archived_from: Option<TaskStatus>,
    pub validation: ValidationReceipt,
    pub agent_exit_code: Option<i32>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    directory: DirectoryIdentity,
}

impl TaskCheckpoint {
    fn from_task(task: &AgentTask) -> io::Result<Self> {
        let record = Self {
            id: task.id,
            title: task.title.clone(),
            provider: task.provider,
            repository: task.repo_root.clone(),
            worktree: task.worktree_path.clone(),
            branch: task.branch.clone(),
            base_commit: task.base_commit.clone(),
            brief: task
                .brief
                .clone()
                .ok_or_else(|| invalid("task has no authored brief"))?,
            recorded_status: task.status,
            archived_from: task.archived_from,
            validation: ValidationReceipt {
                attempt: task.validation.attempt,
                status: task.validation.status,
                exit_code: task.validation.exit_code,
            },
            agent_exit_code: task.exit_code,
            created_at_ms: task.created_at_ms,
            updated_at_ms: task.updated_at_ms,
            directory: DirectoryIdentity::read(&task.worktree_path)?,
        };
        record.validate()?;
        Ok(record)
    }

    fn matches_task_data(&self, task: &AgentTask) -> bool {
        self.id == task.id
            && self.title == task.title
            && self.provider == task.provider
            && self.repository == task.repo_root
            && self.worktree == task.worktree_path
            && self.branch == task.branch
            && self.base_commit == task.base_commit
            && task.brief.as_ref() == Some(&self.brief)
            && self.created_at_ms == task.created_at_ms
            && self.updated_at_ms == task.updated_at_ms
    }

    fn unchanged(&self, task: &AgentTask) -> bool {
        if !self.matches_task_data(task) {
            return false;
        }
        if self.recorded_status == task.status
            && self.archived_from == task.archived_from
            && self.validation.attempt == task.validation.attempt
            && self.validation.status == task.validation.status
            && self.validation.exit_code == task.validation.exit_code
            && self.agent_exit_code == task.exit_code
        {
            return true;
        }
        // No filesystem, serialization or goal cloning on idle GUI ticks.
        let state = match self.recorded_status {
            TaskStatus::Archived => TaskStatus::Archived,
            TaskStatus::Completed => TaskStatus::Completed,
            _ => TaskStatus::Created,
        };
        task.status == state
            && task.runtime_kind == TaskRuntimeKind::Unassigned
            && task.terminal_session_id.is_none()
            && task.exit_code.is_none()
            && task.validation.attempt == self.validation.attempt
            && task.validation.terminal_session_id.is_none()
            && task.validation.exit_code.is_none()
            && task.validation.status
                == if self.validation.attempt == 0 {
                    TaskValidationStatus::NotRun
                } else {
                    TaskValidationStatus::Inconclusive
                }
    }

    fn new_task(&self) -> NewTask {
        NewTask {
            title: self.title.clone(),
            provider: self.provider,
            repo_root: self.repository.clone(),
            worktree_path: self.worktree.clone(),
            branch: self.branch.clone(),
            base_commit: self.base_commit.clone(),
            source_context: None,
        }
    }

    fn validate(&self) -> io::Result<()> {
        super::task::validate_new_task(&self.new_task())
            .map_err(|_| invalid("invalid recovery task metadata"))?;
        self.brief
            .validate()
            .map_err(|_| invalid("invalid recovery brief"))?;
        if self.id.sort_key() == 0 || self.created_at_ms > self.updated_at_ms {
            return Err(invalid("invalid recovery identity or timestamps"));
        }
        for path in [&self.repository, &self.worktree] {
            let text = path
                .to_str()
                .ok_or_else(|| invalid("recovery path must be UTF-8"))?;
            if text.len() > 4096
                || text.chars().any(char::is_control)
                || path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return Err(invalid("invalid recovery path"));
            }
        }
        if self.archived_from == Some(TaskStatus::Archived)
            || (self.recorded_status != TaskStatus::Archived && self.archived_from.is_some())
        {
            return Err(invalid("invalid archived recovery state"));
        }
        // Check raw multiline instructions as well as the serialized envelope:
        // JSON escaping must not conceal a multiline credential from scrubbing.
        for value in std::iter::once(self.title.as_str())
            .chain(std::iter::once(self.brief.goal.as_str()))
            .chain(
                self.brief
                    .validation
                    .iter()
                    .flat_map(|recipe| [recipe.command.as_str(), recipe.shell.as_str()]),
            )
        {
            if crate::redact::redact_secrets(value) != value {
                return Err(invalid(
                    "recovery data contains a recognized secret; remove it before saving",
                ));
            }
        }
        // Never silently alter an executable recipe. Reject recognized secrets
        // rather than save a redacted instruction that could mean something else.
        let json =
            serde_json::to_string(self).map_err(|_| invalid("cannot encode recovery task"))?;
        if crate::redact::redact_secrets(&json) != json {
            return Err(invalid(
                "recovery data contains a recognized secret; remove it before saving",
            ));
        }
        Ok(())
    }

    fn same_identity(&self, other: &Self) -> bool {
        self.id == other.id
            && self.repository == other.repository
            && self.worktree == other.worktree
            && self.branch == other.branch
            && self.base_commit == other.base_commit
            && self.provider == other.provider
            && self.directory == other.directory
            && self.brief == other.brief
            && self.created_at_ms == other.created_at_ms
    }

    /// Fresh filesystem/Git verification; no Git creation, cleanup or execution
    /// of the task recipe. Call on a worker before explicit recovery.
    pub fn verify_workspace(&self, managed_root: &Path) -> io::Result<()> {
        self.validate()?;
        if self.worktree.parent() != Some(managed_root)
            || DirectoryIdentity::read(&self.worktree)? != self.directory
        {
            return Err(invalid("recovery worktree identity changed"));
        }
        WorktreeService::new(managed_root)
            .and_then(|service| {
                service.verify_active_task_worktree(&self.repository, &self.worktree, &self.branch)
            })
            .map_err(|_| invalid("recovery Git worktree verification failed"))
    }

    /// Read-only view when the original worktree cannot be verified. A failed,
    /// unassigned task cannot use native Start or terminal retry gates.
    pub(super) fn unavailable_projection(&self) -> AgentTask {
        let mut task = self.projection();
        task.status = TaskStatus::Failed;
        task.archived_from = None;
        task.status_detail = Some(format!("{} Worktree verification failed; continuation is disabled. Original goal and checkpoint remain available.", task.status_detail.as_deref().unwrap_or("")));
        task
    }

    fn projection(&self) -> AgentTask {
        let finished = self.recorded_status == TaskStatus::Completed;
        let hidden = self.recorded_status == TaskStatus::Archived;
        AgentTask {
            id: self.id, title: self.title.clone(), provider: self.provider,
            status: if hidden { TaskStatus::Archived } else if finished { TaskStatus::Completed }
                else { TaskStatus::Created },
            repo_root: self.repository.clone(), worktree_path: self.worktree.clone(),
            branch: self.branch.clone(), base_commit: self.base_commit.clone(),
            source: None, source_context: None, brief: Some(self.brief.clone()),
            runtime_kind: TaskRuntimeKind::Unassigned, terminal_session_id: None,
            validation: TaskValidationState {
                attempt: self.validation.attempt,
                status: if self.validation.attempt == 0 { TaskValidationStatus::NotRun }
                    else { TaskValidationStatus::Inconclusive },
                ..TaskValidationState::default()
            },
            exit_code: None,
            status_detail: Some(format!(
                "Recovered checkpoint: previous state {}; validation {} (attempt {}, exit {:?}); Agent exit {:?}. No process was resumed. Inspect the worktree and explicitly start a new run; previous approvals and validation do not authorize it.",
                self.recorded_status.label(), self.validation.status.label(), self.validation.attempt,
                self.validation.exit_code, self.agent_exit_code)),
            created_at_ms: self.created_at_ms, updated_at_ms: self.updated_at_ms,
            archived_from: if hidden { Some(if self.archived_from == Some(TaskStatus::Completed) {
                TaskStatus::Completed } else { TaskStatus::Created }) } else { None },
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    records: Vec<TaskCheckpoint>,
}

/// Exclusive store owner. Retain for the lifetime of any recovered runtime;
/// a second window fails closed rather than restoring duplicate tasks.
#[derive(Debug)]
pub struct TaskRecoveryStore {
    root: PathBuf,
    path: PathBuf,
    lease: File,
    records: Vec<TaskCheckpoint>,
    // Exact bytes read/written: detect outside edits before replacing a file.
    revision: Option<String>,
    poisoned: bool,
}

impl TaskRecoveryStore {
    pub fn open(managed_root: &Path) -> io::Result<Self> {
        #[cfg(not(unix))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "recovery requires Unix",
        ));
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            if !managed_root.is_absolute() {
                return Err(invalid("managed root must be absolute"));
            }
            let root = std::fs::canonicalize(managed_root)?;
            let directory = root.join(".task-checkpoints");
            snapshot_file::ensure_private_directory(&directory)?;
            let lease = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(directory.join("owner.lock"))?;
            let metadata = lease.metadata()?;
            // SAFETY: geteuid only reads process identity.
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
                || metadata.nlink() != 1
            {
                return Err(invalid("unsafe recovery lease"));
            }
            // SAFETY: a live descriptor and valid flock operation.
            if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let path = directory.join("tasks.json");
            let revision = read_optional(&path)?;
            let records = match &revision {
                None => Vec::new(),
                Some(text) => decode(text)?,
            };
            Ok(Self {
                root,
                path,
                lease,
                records,
                revision,
                poisoned: false,
            })
        }
    }

    pub fn records(&self) -> &[TaskCheckpoint] {
        &self.records
    }

    /// Durably capture one task. A failed save leaves the previous snapshot
    /// intact. Returns only after file and directory synchronization succeeds.
    pub fn checkpoint(&mut self, task: &AgentTask) -> io::Result<()> {
        if self.poisoned {
            return Err(invalid("reopen recovery store after a failed write"));
        }
        if self
            .records
            .iter()
            .any(|record| record.id == task.id && record.unchanged(task))
        {
            return Ok(());
        }
        let record = TaskCheckpoint::from_task(task)?;
        let mut records = self.records.clone();
        if let Some(old) = records.iter_mut().find(|old| old.id == record.id) {
            if !old.same_identity(&record) {
                return Err(invalid("recovery identity changed"));
            }
            // Merely reopening/inspecting must not replace the historical facts
            // with the deliberately inert projection.
            if &old.projection() == task || old == &record {
                return Ok(());
            }
            if record.updated_at_ms < old.updated_at_ms {
                return Err(invalid("stale recovery checkpoint"));
            }
            *old = record;
        } else {
            if records.len() >= MAX_RECOVERY_TASKS {
                return Err(invalid("recovery store is full"));
            }
            if records.iter().any(|old| old.worktree == record.worktree) {
                return Err(invalid("worktree already has a recovery identity"));
            }
            records.push(record);
        }
        records
            .iter()
            .find(|record| record.id == task.id)
            .unwrap()
            .verify_workspace(&self.root)?;
        self.publish(records)
    }

    /// Explicit reopening only: restore the same TaskId and original goal, but
    /// no process, approval, terminal binding or passing validation authority.
    pub fn reopen(&self, id: TaskId, manager: &mut TaskManager) -> io::Result<TaskId> {
        if self.poisoned {
            return Err(invalid("reopen recovery store after a failed write"));
        }
        if read_optional(&self.path)? != self.revision {
            return Err(invalid("recovery document changed externally"));
        }
        let record = self
            .records
            .iter()
            .find(|record| record.id == id)
            .ok_or_else(|| invalid("unknown recovery task"))?;
        record.verify_workspace(&self.root)?;
        manager
            .insert_recovered(record.projection())
            .map_err(|_| invalid("task already registered"))
    }

    fn publish(&mut self, records: Vec<TaskCheckpoint>) -> io::Result<()> {
        if read_optional(&self.path)? != self.revision {
            self.poisoned = true;
            return Err(invalid("recovery document changed externally"));
        }
        let text = serde_json::to_string(&Document {
            version: VERSION,
            records: records.clone(),
        })
        .map_err(|_| invalid("cannot encode recovery document"))?;
        if text.len() as u64 > MAX_BYTES {
            return Err(invalid("recovery document is too large"));
        }
        if let Err(error) = snapshot_file::write_atomic_private(&self.path, text.as_bytes()) {
            // Publication may have succeeded before directory fsync failed.
            // Do not let in-memory state guess which version is durable.
            self.poisoned = true;
            return Err(error);
        }
        self.revision = Some(text);
        self.records = records;
        Ok(())
    }
}

impl Drop for TaskRecoveryStore {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: explicit unlock also releases a fork-inherited open-file
            // description; the lease file itself is never unlinked/replaced.
            unsafe {
                libc::flock(self.lease.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match snapshot_file::read_bounded_private(path, MAX_BYTES) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn decode(text: &str) -> io::Result<Vec<TaskCheckpoint>> {
    if text.len() as u64 > MAX_BYTES {
        return Err(invalid("recovery document is too large"));
    }
    let document: Document =
        serde_json::from_str(text).map_err(|_| invalid("invalid recovery document"))?;
    if document.version != VERSION || document.records.len() > MAX_RECOVERY_TASKS {
        return Err(invalid("unsupported recovery document version or size"));
    }
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    for record in &document.records {
        record.validate()?;
        if !ids.insert(record.id) || !paths.insert(&record.worktree) {
            return Err(invalid("duplicate recovery identity"));
        }
    }
    Ok(document.records)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::process::Command;

    struct Fixture {
        directory: PathBuf,
        root: PathBuf,
        repository: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let directory =
                std::env::temp_dir().join(format!("jterm-checkpoint-{}", uuid::Uuid::new_v4()));
            let root = directory.join("tasks");
            let repository = directory.join("repo");
            fs::create_dir_all(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            fs::create_dir(&repository).unwrap();
            let fixture = Self {
                directory,
                root,
                repository,
            };
            fixture.git(&["init", "--quiet"]);
            fs::write(fixture.repository.join("tracked"), "baseline").unwrap();
            fixture.git(&["add", "tracked"]);
            fixture.git(&[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "baseline",
            ]);
            fixture
        }
        fn git(&self, args: &[&str]) -> String {
            let output = Command::new("/usr/bin/git")
                .current_dir(&self.repository)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &self.directory)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().into()
        }
        fn task(&self) -> AgentTask {
            let worktree = self.root.join("task-a");
            self.git(&[
                "worktree",
                "add",
                "-qb",
                "task-a",
                worktree.to_str().unwrap(),
            ]);
            let mut manager = TaskManager::new();
            let id = manager
                .create_with_brief(
                    NewTask {
                        title: "恢复任务".into(),
                        provider: super::super::AgentProvider::Codex,
                        repo_root: self.repository.clone(),
                        worktree_path: worktree,
                        branch: "task-a".into(),
                        base_commit: self.git(&["rev-parse", "HEAD"]),
                        source_context: None,
                    },
                    TaskBrief::new(
                        "Keep the original goal\n检查工作树",
                        Some(super::super::TaskValidationRecipe::new("true", "/bin/sh").unwrap()),
                    )
                    .unwrap(),
                )
                .unwrap();
            manager.get(id).unwrap().clone()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn restart_preserves_identity_goal_and_history_without_authority() {
        let f = Fixture::new();
        let mut task = f.task();
        task.status = TaskStatus::WaitingForApproval;
        task.terminal_session_id = Some("old-pty".into());
        task.runtime_kind = TaskRuntimeKind::Native;
        task.validation = TaskValidationState {
            attempt: 4,
            status: TaskValidationStatus::Passed,
            terminal_session_id: Some("validation-pty".into()),
            exit_code: Some(0),
            status_detail: Some("private output".into()),
        };
        task.status_detail = Some("private approval payload".into());
        let mut store = TaskRecoveryStore::open(&f.root).unwrap();
        store.checkpoint(&task).unwrap();
        let text = fs::read_to_string(&store.path).unwrap();
        for excluded in [
            "old-pty",
            "validation-pty",
            "private output",
            "private approval payload",
            "source_context",
            "runtime_kind",
        ] {
            assert!(!text.contains(excluded));
        }
        drop(store);
        let mut store = TaskRecoveryStore::open(&f.root).unwrap();
        let mut manager = TaskManager::new();
        assert_eq!(store.reopen(task.id, &mut manager).unwrap(), task.id);
        let recovered = manager.get(task.id).unwrap();
        assert_eq!(recovered.brief, task.brief);
        assert_eq!(recovered.status, TaskStatus::Created);
        assert_eq!(recovered.runtime_kind, TaskRuntimeKind::Unassigned);
        assert!(recovered.terminal_session_id.is_none());
        assert!(!manager.has_active_agent_event_stream(task.id));
        assert_eq!(
            recovered.validation.status,
            TaskValidationStatus::Inconclusive
        );
        assert_eq!(recovered.validation.attempt, 4);
        assert_eq!(store.reopen(task.id, &mut manager).unwrap(), task.id);
        assert_eq!(manager.tasks().len(), 1);
        store.checkpoint(manager.get(task.id).unwrap()).unwrap();
        assert_eq!(fs::read_to_string(&store.path).unwrap(), text);
    }

    #[test]
    fn duplicate_start_lock_and_external_change_fail_closed() {
        let f = Fixture::new();
        let task = f.task();
        let mut store = TaskRecoveryStore::open(&f.root).unwrap();
        assert!(TaskRecoveryStore::open(&f.root).is_err());
        store.checkpoint(&task).unwrap();
        fs::write(&store.path, "broken").unwrap();
        assert!(store.reopen(task.id, &mut TaskManager::new()).is_err());
        let mut changed = task;
        changed.status = TaskStatus::Working;
        assert!(store.checkpoint(&changed).is_err());
        assert_eq!(fs::read_to_string(&store.path).unwrap(), "broken");
        drop(store);
        assert!(TaskRecoveryStore::open(&f.root).is_err());
    }

    #[test]
    fn incomplete_temp_file_is_ignored_and_future_or_duplicate_records_rejected() {
        let f = Fixture::new();
        let task = f.task();
        let mut store = TaskRecoveryStore::open(&f.root).unwrap();
        store.checkpoint(&task).unwrap();
        let path = store.path.clone();
        fs::write(path.with_extension("tmp"), "{partial").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        drop(store);
        assert_eq!(
            TaskRecoveryStore::open(&f.root).unwrap().records()[0].id,
            task.id
        );
        let mut json: serde_json::Value = serde_json::from_str(&text).unwrap();
        json["version"] = 999.into();
        assert!(decode(&json.to_string()).is_err());
        json["version"] = 1.into();
        let copy = json["records"][0].clone();
        json["records"].as_array_mut().unwrap().push(copy);
        assert!(decode(&json.to_string()).is_err());
        assert!(decode(r#"{"version":1,"version":1,"records":[]}"#).is_err());
        assert!(decode(r#"{"version":1,"records":[],"auto_start":true}"#).is_err());
    }

    #[test]
    fn old_creation_journal_is_preserved_without_fabricating_a_goal() {
        let f = Fixture::new();
        let old = f.root.join(".creation-recovery");
        fs::create_dir(&old).unwrap();
        let bytes = r#"{"version":1,"records":[{"stage":"registered"}]}"#;
        fs::write(old.join("journal.json"), bytes).unwrap();
        let store = TaskRecoveryStore::open(&f.root).unwrap();
        assert!(store.records().is_empty());
        assert_eq!(fs::read_to_string(old.join("journal.json")).unwrap(), bytes);
    }

    #[test]
    fn replaced_worktree_and_changed_identity_are_rejected() {
        let f = Fixture::new();
        let task = f.task();
        let mut store = TaskRecoveryStore::open(&f.root).unwrap();
        store.checkpoint(&task).unwrap();
        let mut changed = task.clone();
        changed.base_commit = "f".repeat(40);
        assert!(store.checkpoint(&changed).is_err());
        fs::rename(&task.worktree_path, f.root.join("old")).unwrap();
        fs::create_dir(&task.worktree_path).unwrap();
        assert!(store.reopen(task.id, &mut TaskManager::new()).is_err());
    }

    #[test]
    fn private_permissions_symlinks_and_recognized_secrets_are_rejected() {
        let f = Fixture::new();
        let mut task = f.task();
        let mut store = TaskRecoveryStore::open(&f.root).unwrap();
        store.checkpoint(&task).unwrap();
        let path = store.path.clone();
        task.brief.as_mut().unwrap().goal =
            "Authorization: Bearer abcdefghijklmnopqrstuvwxyz".into();
        assert!(store.checkpoint(&task).is_err());
        assert!(!fs::read_to_string(&path)
            .unwrap()
            .contains("abcdefghijklmnopqrstuvwxyz"));
        drop(store);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(TaskRecoveryStore::open(&f.root).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let target = path.with_extension("saved");
        fs::rename(&path, &target).unwrap();
        symlink(&target, &path).unwrap();
        assert!(TaskRecoveryStore::open(&f.root).is_err());
    }

    #[test]
    fn all_frontend_names_use_the_same_codec_and_never_restore_live_bindings() {
        let f = Fixture::new();
        let task = f.task();
        for frontend in ["anvil", "forge", "ember", "frost"] {
            let mut record = TaskCheckpoint::from_task(&task).unwrap();
            record.title = frontend.into();
            let text = serde_json::to_string(&Document {
                version: VERSION,
                records: vec![record.clone()],
            })
            .unwrap();
            assert_eq!(decode(&text).unwrap(), vec![record.clone()]);
            for status in [
                TaskStatus::Created,
                TaskStatus::Working,
                TaskStatus::WaitingForHuman,
                TaskStatus::ReadyForReview,
                TaskStatus::Failed,
                TaskStatus::Cancelled,
                TaskStatus::Completed,
                TaskStatus::Archived,
            ] {
                record.recorded_status = status;
                let task = record.projection();
                assert!(!task.status.is_running());
                assert!(task.terminal_session_id.is_none());
                assert_ne!(task.validation.status, TaskValidationStatus::Passed);
            }
        }
    }

    #[test]
    fn manager_restarts_with_same_task_and_saves_creation_before_returning() {
        let f = Fixture::new();
        let task = f.task();
        let mut manager = TaskManager::with_recovery_at(&f.root).unwrap();
        let id = manager
            .create_with_brief(
                NewTask {
                    title: task.title.clone(),
                    provider: task.provider,
                    repo_root: task.repo_root.clone(),
                    worktree_path: task.worktree_path.clone(),
                    branch: task.branch.clone(),
                    base_commit: task.base_commit.clone(),
                    source_context: None,
                },
                task.brief.clone().unwrap(),
            )
            .unwrap();
        assert!(TaskManager::with_recovery_at(&f.root).is_err());
        drop(manager);
        let mut manager = TaskManager::with_recovery_at(&f.root).unwrap();
        assert_eq!(manager.get(id).unwrap().brief, task.brief);
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::Created);
        manager.checkpoint_recovery();
        assert!(manager.recovery_error().is_none());
    }
    #[test]
    fn crash_child() {
        let Some(root) = std::env::var_os("JTERM_TEST_RECOVERY_CRASH_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let mut manager = TaskManager::with_recovery_at(&root).unwrap();
        let id = manager.tasks()[0].id;
        manager
            .update_status(id, TaskStatus::Working, None)
            .unwrap();
        manager.checkpoint_recovery();
        assert!(manager.recovery_error().is_none());
        fs::write(root.join("ready"), b"checkpoint complete").unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn killed_process_releases_lease_and_preserves_last_durable_checkpoint() {
        let f = Fixture::new();
        let task = f.task();
        let mut store = TaskRecoveryStore::open(&f.root).unwrap();
        store.checkpoint(&task).unwrap();
        drop(store);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent_task::recovery::tests::crash_child",
                "--nocapture",
            ])
            .env("JTERM_TEST_RECOVERY_CRASH_ROOT", &f.root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !f.root.join("ready").exists() && std::time::Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("crash child exited early: {status}");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let ready = f.root.join("ready").exists();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(ready);
        let store = TaskRecoveryStore::open(&f.root).unwrap();
        assert_eq!(store.records()[0].recorded_status, TaskStatus::Working);
        let mut manager = TaskManager::new();
        store.reopen(task.id, &mut manager).unwrap();
        assert_eq!(manager.get(task.id).unwrap().status, TaskStatus::Created);
        assert!(!manager.has_active_agent_event_stream(task.id));
    }

    #[test]
    fn explicit_restart_creates_a_fresh_epoch_without_replaying_approvals() {
        let f = Fixture::new();
        let mut task = f.task();
        task.status = TaskStatus::WaitingForApproval;
        let mut store = TaskRecoveryStore::open(&f.root).unwrap();
        store.checkpoint(&task).unwrap();
        let mut manager = TaskManager::new();
        store.reopen(task.id, &mut manager).unwrap();
        assert!(!manager.has_active_agent_event_stream(task.id));
        let stream = manager.start_agent_event_stream(task.id).unwrap();
        assert_eq!(stream.task_id(), task.id);
        assert!(manager.has_active_agent_event_stream(task.id));
        assert!(store.reopen(task.id, &mut manager).is_err());
    }
    #[test]
    fn unavailable_worktree_keeps_goal_visible_but_cannot_start() {
        let f = Fixture::new();
        let task = f.task();
        let mut store = TaskRecoveryStore::open(&f.root).unwrap();
        store.checkpoint(&task).unwrap();
        let original = fs::read_to_string(&store.path).unwrap();
        let path = store.path.clone();
        drop(store);
        fs::rename(&task.worktree_path, f.root.join("moved")).unwrap();
        let mut manager = TaskManager::with_recovery_at(&f.root).unwrap();
        assert_eq!(manager.get(task.id).unwrap().brief, task.brief);
        assert_eq!(manager.get(task.id).unwrap().status, TaskStatus::Failed);
        assert!(manager.start_agent_event_stream(task.id).is_err());
        assert!(manager.native_terminal_fallback_eligible(task.id).is_err());
        manager.checkpoint_recovery();
        assert_eq!(fs::read_to_string(path).unwrap(), original);
        assert!(manager.take_recovery_notice().is_some());
    }
}
