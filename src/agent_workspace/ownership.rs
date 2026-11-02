use super::model::{ConversationId, RuntimeSnapshot};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub(super) struct Paths {
    pub state: PathBuf,
    pub runtime: PathBuf,
    pub socket: PathBuf,
}

impl Paths {
    pub fn locate() -> Result<Self, String> {
        let state = std::env::var_os("JTERM_WORKSPACE_STATE_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("XDG_STATE_HOME")
                    .map(|p| PathBuf::from(p).join("jterm/agent-workspace"))
            })
            .or_else(|| dirs::home_dir().map(|p| p.join(".local/state/jterm/agent-workspace")))
            .ok_or("Cannot locate a private workspace state directory")?;
        Self::located(state)
    }
    pub fn discover() -> Result<Self, String> {
        let paths = Self::locate()?;
        Self::at(paths.state)
    }
    pub fn at(state: PathBuf) -> Result<Self, String> {
        let paths = Self::located(state)?;
        private_dir(&paths.state)?;
        private_dir(&paths.runtime)?;
        private_dir(&paths.runtime.join("leases"))?;
        Ok(paths)
    }
    fn located(state: PathBuf) -> Result<Self, String> {
        if !state.is_absolute() {
            return Err("Workspace state directory must be absolute".into());
        }
        let runtime = if state.as_os_str().len() < 70 {
            state.join("run")
        } else {
            std::env::temp_dir().join(format!(
                "jterm-session-{}-{}",
                unsafe { libc::geteuid() },
                file_key(&state.to_string_lossy())
            ))
        };
        let socket = runtime.join("host.sock");
        Ok(Self {
            state,
            runtime,
            socket,
        })
    }
    pub fn lease(&self, id: &ConversationId) -> PathBuf {
        self.runtime
            .join("leases")
            .join(format!("{}.lock", file_key(&id.0)))
    }
}

pub(super) fn private_dir(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::geteuid() }
            {
                return Err(format!(
                    "Workspace path is not a private directory: {}",
                    path.display()
                ));
            }
            if metadata.mode() & 0o077 != 0 {
                return Err(format!(
                    "Workspace directory must have mode 0700: {}",
                    path.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Parents may be ordinary user state directories. The final
            // component is always checked again after mkdir (including races).
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|e| e.to_string())?;
            let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                return Err("Unsafe workspace directory".into());
            }
        }
        Err(error) => return Err(error.to_string()),
    }
    Ok(())
}

pub(super) fn private_file(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1
    {
        return Err("Workspace file has an unsafe owner or file type".into());
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    Ok(file)
}

/// An advisory lock is retained as an open file description, not a heartbeat
/// timeout. Children inherit it so a host crash cannot grant a second writer
/// while an orphaned provider still owns this session.
pub(super) struct Lease {
    file: File,
}

impl Lease {
    pub fn acquire(path: &Path) -> Result<Self, String> {
        let file = private_file(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("This session already has a live input owner".into());
        }
        Ok(Self { file })
    }
    pub fn write(&mut self, record: &impl Serialize) -> Result<(), String> {
        let bytes = serde_json::to_vec(record).map_err(|e| e.to_string())?;
        self.file
            .seek(SeekFrom::Start(0))
            .and_then(|_| self.file.set_len(0))
            .and_then(|_| self.file.write_all(&bytes))
            .and_then(|_| self.file.sync_data())
            .map_err(|e| e.to_string())
    }
    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
    pub fn configure_child(&self, command: &mut std::process::Command) -> Result<(), String> {
        let fd = self.fd();
        // Only the child's descriptor table is changed. The parent's lock
        // stays CLOEXEC even if other threads spawn unrelated subprocesses.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_DUPFD, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }
}

/// Cross-process input ownership for a native structured provider session.
/// Keep this guard for the entire conversation runtime, including idle gaps
/// between provider subprocesses. Use configure_child before every spawn so
/// a parent crash cannot admit a second writer while its child survives.
pub struct NativeSessionLease {
    _lease: Lease,
    pub id: ConversationId,
}

impl std::fmt::Debug for NativeSessionLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSessionLease")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl NativeSessionLease {
    pub(super) fn into_lease(self) -> Lease {
        self._lease
    }
    pub(super) fn acquire_task(paths: &Paths, id: ConversationId) -> Result<Self, String> {
        let mut lease = Lease::acquire(&paths.lease(&id))?;
        let pid = std::process::id();
        let start = process_start(pid).ok_or("Cannot identify native host process")?;
        lease.write(&LeaseRecord {
            id: id.clone(),
            host_pid: pid,
            host_start: start.clone(),
            runtime: RuntimeSnapshot {
                cleanup_verified: false,
                id: id.clone(),
                state: super::model::RuntimeState::Running,
                pid,
                process_start: start,
                epoch: uuid::Uuid::new_v4().to_string(),
                event_sequence: 0,
                cols: 0,
                rows: 0,
                exit_code: None,
                detail: "Host-owned structured native execution".into(),
            },
        })?;
        Ok(Self { _lease: lease, id })
    }
    pub fn acquire(
        provider: crate::agent_sessions::SessionProvider,
        provider_home: &Path,
        native_id: &str,
    ) -> Result<Self, String> {
        Self::acquire_paths(Paths::discover()?, provider, provider_home, native_id)
    }
    pub fn acquire_at(
        state_dir: impl AsRef<Path>,
        provider: crate::agent_sessions::SessionProvider,
        provider_home: &Path,
        native_id: &str,
    ) -> Result<Self, String> {
        Self::acquire_paths(
            Paths::at(state_dir.as_ref().to_path_buf())?,
            provider,
            provider_home,
            native_id,
        )
    }
    fn acquire_paths(
        paths: Paths,
        provider: crate::agent_sessions::SessionProvider,
        provider_home: &Path,
        native_id: &str,
    ) -> Result<Self, String> {
        let id = super::model::source_id(
            provider.label(),
            &super::model::local_host_id(),
            provider_home,
            native_id,
        )?;
        let mut lease = Lease::acquire(&paths.lease(&id))?;
        let pid = std::process::id();
        let start =
            process_start(pid).ok_or("Cannot verify the native runtime's process identity")?;
        lease.write(&LeaseRecord {
            id: id.clone(),
            host_pid: 0,
            host_start: String::new(),
            runtime: RuntimeSnapshot {
                cleanup_verified: false,
                id: id.clone(),
                state: super::model::RuntimeState::Running,
                pid,
                process_start: start,
                epoch: uuid::Uuid::new_v4().to_string(),
                event_sequence: 0,
                cols: 0,
                rows: 0,
                exit_code: None,
                detail: "Native structured session owner".into(),
            },
        })?;
        Ok(Self { _lease: lease, id })
    }
    /// The guard must remain alive until the configured child has spawned.
    pub fn configure_child(&self, command: &mut std::process::Command) -> Result<(), String> {
        self._lease.configure_child(command)
    }
}

// Do not call LOCK_UN in Drop: the provider inherits this open-file
// description. Closing the final descriptor releases the kernel lease.

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct LeaseRecord {
    pub id: ConversationId,
    pub host_pid: u32,
    pub host_start: String,
    pub runtime: RuntimeSnapshot,
}

pub(super) fn read_lease(path: &Path) -> Option<LeaseRecord> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.len() > 65536 {
        return None;
    }
    let mut bytes = String::new();
    file.take(65537).read_to_string(&mut bytes).ok()?;
    serde_json::from_str(&bytes).ok()
}

/// A retained record is not proof of retained ownership: a native runtime
/// can release its guard while the frontend process remains alive.
pub(super) fn lease_is_held(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    else {
        return false;
    };
    if file
        .metadata()
        .ok()
        .is_none_or(|m| !m.is_file() || m.uid() != unsafe { libc::geteuid() })
    {
        return false;
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return false;
    }
    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EWOULDBLOCK)
    )
}

pub(super) fn process_start(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = stat.rsplit_once(')')?.1;
        let fields: Vec<_> = rest.split_whitespace().collect();
        if fields.first().is_some_and(|s| *s == "Z") {
            return None;
        }
        fields.get(19).map(|s| (*s).to_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

pub(super) fn process_matches(pid: u32, start: &str) -> bool {
    !start.is_empty() && process_start(pid).as_deref() == Some(start)
}

pub(super) fn file_key(value: &str) -> String {
    // A filename only: the complete identity is retained and checked in the
    // lease and database. Even a hash collision fails closed, never aliases.
    let mut a = 0xcbf29ce484222325u64;
    let mut b = 0x84222325cbf29ce4u64;
    for byte in value.bytes() {
        a = (a ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        b = (b ^ u64::from(byte))
            .rotate_left(7)
            .wrapping_mul(0x9e3779b185ebca87);
    }
    format!("{a:016x}{b:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kernel_lease_excludes_independent_open_and_releases_on_close() {
        if super::super::isolated_lease_test(
            "agent_workspace::ownership::tests::kernel_lease_excludes_independent_open_and_releases_on_close",
        ) {
            return;
        }
        let root = std::env::temp_dir().join(format!("jterm-lease-{}", uuid::Uuid::new_v4()));
        private_dir(&root).unwrap();
        let path = root.join("owner");
        let first = Lease::acquire(&path).unwrap();
        assert!(Lease::acquire(&path).is_err());
        drop(first);
        assert!(Lease::acquire(&path).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn process_identity_rejects_pid_reuse() {
        let pid = std::process::id();
        let start = process_start(pid).unwrap();
        assert!(process_matches(pid, &start));
        assert!(!process_matches(pid, "different-start"));
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn inherited_native_lease_survives_guard_drop_until_child_exits() {
        const PROBE: &str = "JTERM_INHERITED_LEASE_PROBE";
        if std::env::var_os(PROBE).is_none() {
            // CLOEXEC closes a descriptor at exec, not fork. Other tests in
            // this process deliberately pause children in pre_exec, so they
            // can transiently inherit our lock while the suite runs in
            // parallel. Start an isolated probe before acquiring the lease;
            // below we exercise that pre-exec boundary deterministically.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "agent_workspace::ownership::tests::inherited_native_lease_survives_guard_drop_until_child_exits",
                    "--nocapture",
                ])
                .env(PROBE, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated lease probe failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        use std::os::fd::FromRawFd;
        use std::time::Duration;

        fn pipe() -> (File, File) {
            let mut fds = [-1; 2];
            // Atomic CLOEXEC also prevents these test barriers escaping into
            // the provider. Their only non-exec user is the gated child.
            assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
            unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
        }

        let root = std::env::temp_dir().join(format!("jterm-inherited-{}", uuid::Uuid::new_v4()));
        let home = root.join("provider");
        std::fs::create_dir_all(&home).unwrap();
        let guard = NativeSessionLease::acquire_at(
            root.join("state"),
            crate::agent_sessions::SessionProvider::Claude,
            &home,
            "session",
        )
        .unwrap();
        let mut command = std::process::Command::new("sleep");
        command.arg("5");
        guard.configure_child(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        drop(guard);
        assert!(NativeSessionLease::acquire_at(
            root.join("state"),
            crate::agent_sessions::SessionProvider::Claude,
            &home,
            "session"
        )
        .is_err());
        child.kill().unwrap();
        child.wait().unwrap();
        let guard = NativeSessionLease::acquire_at(
            root.join("state"),
            crate::agent_sessions::SessionProvider::Claude,
            &home,
            "session",
        )
        .expect("the configured provider's inherited lease must release after reap");

        let (mut ready_read, ready_write) = pipe();
        let (gate_read, mut gate_write) = pipe();
        let ready_read_fd = ready_read.as_raw_fd();
        let gate_write_fd = gate_write.as_raw_fd();
        let unrelated = std::thread::spawn(move || {
            let ready_fd = ready_write.as_raw_fd();
            let gate_fd = gate_read.as_raw_fd();
            let mut command = std::process::Command::new("sleep");
            command.arg("5");
            // Deliberately do NOT configure_child: this process inherits only
            // the parent's ordinary CLOEXEC descriptor during fork.
            unsafe {
                command.pre_exec(move || {
                    libc::close(ready_read_fd);
                    libc::close(gate_write_fd);
                    let byte = 1u8;
                    if libc::write(ready_fd, &byte as *const u8 as *const libc::c_void, 1) != 1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let mut poll = libc::pollfd {
                        fd: gate_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // A failed test cannot leave an orphan stuck in pre_exec.
                    if libc::poll(&mut poll, 1, 5000) != 1 {
                        return Err(std::io::Error::from_raw_os_error(libc::ETIMEDOUT));
                    }
                    let mut released = 0u8;
                    if libc::read(gate_fd, &mut released as *mut u8 as *mut libc::c_void, 1) != 1 {
                        return Err(std::io::Error::from_raw_os_error(libc::EPIPE));
                    }
                    Ok(())
                });
            }
            // Keep the closure's descriptors alive until spawn finishes its
            // fork/exec handshake; all calls inside pre_exec are libc only.
            let spawned = command.spawn();
            drop((ready_write, gate_read));
            spawned
        });
        let mut ready = libc::pollfd {
            fd: ready_read.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut ready, 1, Duration::from_secs(3).as_millis() as i32) },
            1
        );
        ready_read.read_exact(&mut [0u8; 1]).unwrap();
        drop(guard);
        // This second ownership cycle has no configured provider at all;
        // only the unrelated pre-exec child retains a CLOEXEC reference.
        // Releasing early would weaken the real kernel ownership contract.
        assert!(NativeSessionLease::acquire_at(
            root.join("state"),
            crate::agent_sessions::SessionProvider::Claude,
            &home,
            "session"
        )
        .is_err());
        gate_write.write_all(&[1]).unwrap();
        let mut unrelated = unrelated.join().unwrap().unwrap();
        assert!(unrelated.try_wait().unwrap().is_none());
        let released = NativeSessionLease::acquire_at(
            root.join("state"),
            crate::agent_sessions::SessionProvider::Claude,
            &home,
            "session",
        )
        .expect("exec must close the last inherited CLOEXEC lease descriptor");
        drop(released);
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
