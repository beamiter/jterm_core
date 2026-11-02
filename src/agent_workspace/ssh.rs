//! Explicit SSH observation of an already-running remote workspace host.
//! There is no remote launch, control, mutation or automatic reconnect/retry.
//! Query text travels only in a bounded JSON stdin frame after host identity
//! and protocol verification. The SSH command line is fixed except for the
//! validated alias and executable pathname.

use super::protocol::{read_frame, write_frame, MAX_FRAME_BYTES};
use super::{WorkspaceCommand, WorkspaceReply, PROTOCOL_VERSION};
use crate::supervised::SupervisedChild;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

pub(super) const COMMAND_STDIO_FLAG: &str = "--jterm-command-stdio";
pub(super) const HOST_ID_FLAG: &str = "--jterm-workspace-host-id";
const SSH_TIMEOUT: Duration = Duration::from_secs(75);
const HELLO_LIMIT: usize = 8192;
const DIAGNOSTIC_LIMIT: usize = 4096;

/// An explicitly configured SSH alias, installed remote executable and pinned
/// workspace host identity. Obtain the identity by running the remote program
/// with `--jterm-workspace-host-id` on that machine. SSH known-host verification
/// is independently required; the workspace identity does not replace it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSshHost {
    pub alias: String,
    pub remote_executable: PathBuf,
    pub expected_host_id: String,
    ssh_executable: PathBuf,
}

impl WorkspaceSshHost {
    pub fn new(
        alias: impl Into<String>,
        remote_executable: impl AsRef<Path>,
        expected_host_id: impl Into<String>,
    ) -> Result<Self, String> {
        let host = Self {
            alias: alias.into(),
            remote_executable: remote_executable.as_ref().to_path_buf(),
            expected_host_id: expected_host_id.into(),
            ssh_executable: PathBuf::from("/usr/bin/ssh"),
        };
        host.validate()?;
        Ok(host)
    }

    /// Explicit local SSH executable for alternate packaging and fixture
    /// transports. Construction performs no network or filesystem access.
    pub fn with_ssh_executable(mut self, executable: impl AsRef<Path>) -> Result<Self, String> {
        self.ssh_executable = executable.as_ref().to_path_buf();
        self.validate()?;
        Ok(self)
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        if self.alias.is_empty()
            || self.alias.len() > 255
            || self.alias.starts_with('-')
            || !self
                .alias
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err("Choose an explicit SSH configuration alias containing only letters, digits, dots, underscores or hyphens".into());
        }
        let remote = self
            .remote_executable
            .to_str()
            .ok_or("Remote executable must be UTF-8")?;
        // OpenSSH invokes a remote login shell. Keep the executable a single
        // inert shell word; no shell quoting or user query interpolation occurs.
        if !self.remote_executable.is_absolute()
            || remote.len() > 4096
            || remote.ends_with('/')
            || !remote
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
            || self
                .remote_executable
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        {
            return Err("Remote executable must be an absolute path with no spaces, parent traversal or shell metacharacters".into());
        }
        if !valid_host_identity(&self.expected_host_id) {
            return Err(
                "Provide the exact bounded host ID reported by the remote workspace executable"
                    .into(),
            );
        }
        if !self.ssh_executable.is_absolute() || self.ssh_executable.as_os_str().is_empty() {
            return Err("The local SSH executable must be an explicit absolute path".into());
        }
        Ok(())
    }

    pub(super) fn request(&self, command: WorkspaceCommand) -> Result<WorkspaceReply, String> {
        self.request_with_timeout(command, SSH_TIMEOUT)
    }

    fn request_with_timeout(
        &self,
        command: WorkspaceCommand,
        timeout: Duration,
    ) -> Result<WorkspaceReply, String> {
        self.validate()?;
        let expected_reply = read_only_kind(&command).ok_or_else(read_only_error)?;
        let mut ssh = Command::new(&self.ssh_executable);
        ssh.args([
            "-T",
            "-oBatchMode=yes",
            "-oStrictHostKeyChecking=yes",
            "-oConnectTimeout=10",
            "-oServerAliveInterval=5",
            "-oServerAliveCountMax=2",
            "-oClearAllForwardings=yes",
            "-oForwardAgent=no",
            "-oForwardX11=no",
            "-oPermitLocalCommand=no",
            "--",
            self.alias.as_str(),
        ])
        .arg(&self.remote_executable)
        .arg(COMMAND_STDIO_FLAG)
        .env_remove("JTERM_WORKSPACE_STATE_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut transport = SshTransport::spawn(&mut ssh, timeout)?;
        let result = (|| {
            let hello: RemoteHello = transport.read(HELLO_LIMIT)?;
            verify_hello(&hello, &self.expected_host_id)?;
            if !transport.pending.is_empty() {
                return Err("Remote workspace sent data before receiving a request".into());
            }
            let request_id = uuid::Uuid::new_v4().to_string();
            let request = RemoteRequest {
                version: PROTOCOL_VERSION,
                expected_host_id: self.expected_host_id.clone(),
                request_id: request_id.clone(),
                command,
            };
            // Nothing from the request is sent before the identity handshake.
            transport.write(&request)?;
            let response: RemoteResponse = transport.read(MAX_FRAME_BYTES)?;
            if response.version != PROTOCOL_VERSION
                || response.host_id != self.expected_host_id
                || response.request_id != request_id
            {
                return Err("Remote workspace response identity or protocol changed; request was not retried".into());
            }
            transport.finish()?;
            match response.result {
                Ok(reply) if expected_reply.matches(&reply) => Ok(reply),
                Ok(_) => Err(
                    "Remote workspace returned an unexpected reply; no local action was taken"
                        .into(),
                ),
                Err(error) => Err(crate::review_input::safe_inline_display(
                    &crate::redact::redact_secrets(&error),
                    2048,
                )),
            }
        })();
        result.map_err(|error| transport.diagnostic(&error))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct RemoteHello {
    version: u32,
    host_id: String,
    mode: String,
}
#[derive(Debug, Serialize, Deserialize)]
struct RemoteRequest {
    version: u32,
    expected_host_id: String,
    request_id: String,
    command: WorkspaceCommand,
}
#[derive(Debug, Serialize, Deserialize)]
struct RemoteResponse {
    version: u32,
    host_id: String,
    request_id: String,
    result: Result<WorkspaceReply, String>,
}

fn valid_host_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && crate::review_input::validate(value).is_ok()
}
fn verify_hello(hello: &RemoteHello, expected: &str) -> Result<(), String> {
    if hello.version != PROTOCOL_VERSION || hello.mode != "read-only" {
        return Err(
            "Remote workspace protocol mismatch; update the remote host and frontend together"
                .into(),
        );
    }
    if hello.host_id != expected {
        return Err("Remote workspace host identity mismatch; no query was sent".into());
    }
    Ok(())
}
fn read_only_error() -> String {
    "SSH Workspace is read-only. Starting, attaching, sending, approving, stopping and all metadata changes are unavailable through this connection".into()
}

#[derive(Clone, Copy)]
enum ReadOnlyKind {
    Catalog,
    CatalogPage,
    Search,
    SearchDocument,
    Context,
    Result,
    Queue,
}
impl ReadOnlyKind {
    fn matches(self, reply: &WorkspaceReply) -> bool {
        matches!(
            (self, reply),
            (Self::Catalog, WorkspaceReply::Catalog { .. })
                | (Self::CatalogPage, WorkspaceReply::CatalogPage { .. })
                | (Self::Search, WorkspaceReply::Search { .. })
                | (Self::SearchDocument, WorkspaceReply::SearchDocument { .. })
                | (Self::Context, WorkspaceReply::Context { .. })
                | (Self::Result, WorkspaceReply::Result { .. })
                | (Self::Queue, WorkspaceReply::Queue { .. })
        )
    }
}
fn read_only_kind(command: &WorkspaceCommand) -> Option<ReadOnlyKind> {
    Some(match command {
        WorkspaceCommand::Refresh => ReadOnlyKind::Catalog,
        WorkspaceCommand::CatalogPage { .. } => ReadOnlyKind::CatalogPage,
        WorkspaceCommand::Search { .. } => ReadOnlyKind::Search,
        WorkspaceCommand::ReadSearchDocument { .. } => ReadOnlyKind::SearchDocument,
        WorkspaceCommand::ReadContext { .. } => ReadOnlyKind::Context,
        WorkspaceCommand::InspectResult { .. } => ReadOnlyKind::Result,
        WorkspaceCommand::QueueSnapshot => ReadOnlyKind::Queue,
        _ => return None,
    })
}
pub(super) fn is_read_only_command(command: &WorkspaceCommand) -> bool {
    read_only_kind(command).is_some()
}

/// One stdin command, one stdout response. Both endpoints enforce the same
/// read-only allowlist. The remote helper never creates or starts a host.
pub(super) fn run_stdio() -> Result<(), String> {
    let host_id = super::local_host_id();
    if !valid_host_identity(&host_id) {
        return Err("This machine has no usable workspace host identity".into());
    }
    let mut output = std::io::stdout().lock();
    write_frame(
        &mut output,
        &RemoteHello {
            version: PROTOCOL_VERSION,
            host_id: host_id.clone(),
            mode: "read-only".into(),
        },
    )?;
    let request: RemoteRequest =
        read_frame(&mut std::io::stdin().lock())?.ok_or("No remote workspace command supplied")?;
    let result = (|| {
        if request.version != PROTOCOL_VERSION || request.expected_host_id != host_id {
            return Err("Remote workspace identity or protocol mismatch".into());
        }
        if uuid::Uuid::parse_str(&request.request_id).is_err() {
            return Err("Invalid remote workspace request identity".into());
        }
        let expected = read_only_kind(&request.command).ok_or_else(read_only_error)?;
        let paths = super::ownership::Paths::locate()?;
        let stream = super::client::connect(&paths, false, None)?;
        let reply = super::client::command_request(stream, request.command)?;
        if !expected.matches(&reply) {
            return Err("Workspace host returned an unexpected read-only reply".into());
        }
        Ok(reply)
    })();
    write_frame(
        &mut output,
        &RemoteResponse {
            version: PROTOCOL_VERSION,
            host_id,
            request_id: request.request_id,
            result,
        },
    )
}

struct SshTransport {
    child: SupervisedChild,
    input: Option<ChildStdin>,
    output: ChildStdout,
    errors: ChildStderr,
    pending: Vec<u8>,
    diagnostics: Vec<u8>,
    deadline: Instant,
}
impl SshTransport {
    fn spawn(command: &mut Command, timeout: Duration) -> Result<Self, String> {
        let mut child = SupervisedChild::spawn(command)
            .map_err(|error| format!("Could not start SSH observation: {error}"))?;
        let input = child.take_stdin().ok_or("SSH stdin unavailable")?;
        let output = child.take_stdout().ok_or("SSH stdout unavailable")?;
        let errors = child.take_stderr().ok_or("SSH stderr unavailable")?;
        for fd in [input.as_raw_fd(), output.as_raw_fd(), errors.as_raw_fd()] {
            // These are exclusively owned pipe descriptors. Nonblocking I/O
            // keeps a malicious or disconnected peer within the same deadline.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        Ok(Self {
            child,
            input: Some(input),
            output,
            errors,
            pending: Vec::new(),
            diagnostics: Vec::new(),
            deadline: Instant::now() + timeout,
        })
    }
    fn check_deadline(&self) -> Result<(), String> {
        if Instant::now() >= self.deadline {
            Err("SSH workspace request timed out; it was not retried".into())
        } else {
            Ok(())
        }
    }
    fn drain_errors(&mut self) {
        let mut bytes = [0u8; 4096];
        for _ in 0..8 {
            match self.errors.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => {
                    let keep = count.min(DIAGNOSTIC_LIMIT.saturating_sub(self.diagnostics.len()));
                    self.diagnostics.extend_from_slice(&bytes[..keep]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }
    fn read<T: serde::de::DeserializeOwned>(&mut self, limit: usize) -> Result<T, String> {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            self.check_deadline()?;
            self.drain_errors();
            if let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                if end + 1 > limit {
                    return Err("Remote workspace frame exceeds its limit".into());
                }
                let frame: Vec<_> = self.pending.drain(..=end).collect();
                return serde_json::from_slice(&frame)
                    .map_err(|_| "Remote workspace returned invalid JSON".into());
            }
            if self.pending.len() > limit {
                return Err("Remote workspace frame exceeds its limit".into());
            }
            match self.output.read(&mut chunk) {
                Ok(0) => return Err(
                    "SSH workspace connection closed before its response; request was not retried"
                        .into(),
                ),
                Ok(count) => self.pending.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => return Err(format!("SSH workspace output failed: {error}")),
            }
        }
    }
    fn write(&mut self, request: &RemoteRequest) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(request).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        if bytes.len() > MAX_FRAME_BYTES {
            return Err("Remote workspace request exceeds its frame budget".into());
        }
        let mut written = 0;
        while written < bytes.len() {
            self.check_deadline()?;
            self.drain_errors();
            match self
                .input
                .as_mut()
                .ok_or("SSH stdin closed")?
                .write(&bytes[written..])
            {
                Ok(0) => {
                    return Err(
                        "SSH workspace stopped accepting its request; it was not retried".into(),
                    )
                }
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => {
                    return Err(format!(
                        "SSH workspace input failed; request was not retried: {error}"
                    ))
                }
            }
        }
        self.input.take();
        Ok(())
    }
    fn finish(&mut self) -> Result<(), String> {
        loop {
            self.check_deadline()?;
            self.drain_errors();
            if self
                .child
                .root_has_exited()
                .map_err(|error| error.to_string())?
            {
                let status = self
                    .child
                    .reap_after_group_kill()
                    .map_err(|error| error.to_string())?;
                return if status.success() {
                    Ok(())
                } else {
                    Err("SSH workspace exited unsuccessfully; request was not retried".into())
                };
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn diagnostic(&mut self, error: &str) -> String {
        self.drain_errors();
        let detail = crate::review_input::safe_inline_display(
            &crate::redact::redact_secrets(&String::from_utf8_lossy(&self.diagnostics)),
            1024,
        );
        if detail.trim().is_empty() {
            error.into()
        } else {
            format!("{error}: {detail}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn alias_executable_and_identity_are_inert_explicit_configuration() {
        assert!(WorkspaceSshHost::new(
            "build-host",
            "/opt/jterm/bin/jterm-sessiond",
            "fixture-host"
        )
        .is_ok());
        for alias in ["-oProxyCommand=x", "host;id", "host name", "user@host", ""] {
            assert!(WorkspaceSshHost::new(alias, "/bin/jterm-sessiond", "fixture").is_err());
        }
        for executable in [
            "jterm-sessiond",
            "/bin/a b",
            "/bin/$(id)",
            "/bin/a;id",
            "/bin/../a",
        ] {
            assert!(WorkspaceSshHost::new("host", executable, "fixture").is_err());
        }
        assert!(WorkspaceSshHost::new("host", "/bin/jterm-sessiond", "host\nother").is_err());
        assert!(!is_read_only_command(&WorkspaceCommand::CreateChat {
            title: "No".into()
        }));
        assert!(!is_read_only_command(&WorkspaceCommand::QueueUpdate {
            update: super::super::WorkspaceQueueUpdate {
                expected_revision: 0,
                change: super::super::WorkspaceQueueChange::Remove {
                    id: super::super::ConversationId("task".into())
                }
            }
        }));
    }
    #[test]
    fn a_stalled_ssh_child_is_bounded_and_not_retried() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("jterm-ssh-timeout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("ssh");
        let attempts = root.join("attempts");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf x >> '{}'\nsleep 60\n",
                attempts.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = WorkspaceSshHost::new("fixture", "/bin/jterm-sessiond", "fixture")
            .unwrap()
            .with_ssh_executable(script)
            .unwrap();
        let started = Instant::now();
        assert!(config
            .request_with_timeout(WorkspaceCommand::Refresh, Duration::from_millis(80))
            .unwrap_err()
            .contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(std::fs::read(&attempts).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(root);
    }
}
