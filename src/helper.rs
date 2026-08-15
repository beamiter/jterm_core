//! Bounded boundary for the helpers a jterm starts automatically.
//!
//! These integrations must not inherit command resolution from the shell: a
//! project-local executable or a user-writable PATH entry must never decide
//! what the terminal starts in the background. Every [`TrustedHelper`] is
//! resolved from fixed absolute system candidates, canonicalised before exec,
//! and every invocation is output- and time-bounded while this process owns
//! the helper's whole process group through [`crate::supervised`].
//!
//! The trust policy here is deliberately stricter than the PATH-based lookup
//! in [`crate::host`]: a component owned by a third user fails closed even
//! when it is not writable, because an automatic helper must name a system
//! executable, not merely a file nobody can currently modify.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const FONT_HELPER_TIMEOUT: Duration = Duration::from_secs(5);
const NOTIFICATION_HELPER_TIMEOUT: Duration = Duration::from_secs(3);
const FONT_LIST_STDOUT_LIMIT: usize = 4 * 1024 * 1024;
const FONT_MATCH_STDOUT_LIMIT: usize = 64 * 1024;
const HELPER_STDERR_LIMIT: usize = 64 * 1024;
const NOTIFICATION_OUTPUT_LIMIT: usize = 16 * 1024;
/// A helper's own subprocesses resolve through this fixed list, never through
/// the user's PATH.
const TRUSTED_CHILD_PATH: &str = "/usr/bin:/bin";

/// A named automatic helper resolved from fixed absolute system candidates.
///
/// The candidate list is part of the trust decision: it is the complete set
/// of pathnames this process will ever execute for the helper, in preference
/// order.
pub struct TrustedHelper {
    name: &'static str,
    candidates: &'static [&'static str],
}

/// Fontconfig listing used for configured-family fallback probes.
pub const FC_LIST: TrustedHelper = TrustedHelper::new(
    "fc-list",
    &["/usr/bin/fc-list", "/bin/fc-list", "/usr/local/bin/fc-list"],
);
/// Fontconfig family-to-file resolution.
pub const FC_MATCH: TrustedHelper = TrustedHelper::new(
    "fc-match",
    &[
        "/usr/bin/fc-match",
        "/bin/fc-match",
        "/usr/local/bin/fc-match",
    ],
);
/// Desktop notification bridge for app-driven (OSC 9 / OSC 777) toasts.
pub const NOTIFY_SEND: TrustedHelper = TrustedHelper::new(
    "notify-send",
    &[
        "/usr/bin/notify-send",
        "/bin/notify-send",
        "/usr/local/bin/notify-send",
    ],
);

impl TrustedHelper {
    pub const fn new(name: &'static str, candidates: &'static [&'static str]) -> Self {
        Self { name, candidates }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Resolve to the canonical target of the first trusted candidate.
    pub fn resolve(&self) -> Option<PathBuf> {
        self.candidates
            .iter()
            .find_map(|candidate| trusted_system_executable(Path::new(candidate)))
    }

    /// Run the helper with both streams captured under independent byte limits
    /// and one absolute deadline.
    ///
    /// The child executes with `PATH` clamped to the fixed system list and a
    /// null stdin. A non-success exit status is reported as an error carrying
    /// the helper name; the captured output is discarded with it.
    pub fn run<I, S>(
        &self,
        args: I,
        stdout_limit: usize,
        stderr_limit: usize,
        timeout: Duration,
    ) -> io::Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let program = self.resolve().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no trusted {} executable is available", self.name),
            )
        })?;
        let mut command = Command::new(program);
        command.args(args).env("PATH", TRUSTED_CHILD_PATH);
        let output = bounded_command_output(&mut command, stdout_limit, stderr_limit, timeout)?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(io::Error::other(format!(
                "{} exited unsuccessfully ({})",
                self.name, output.status
            )))
        }
    }
}

/// Run `fc-list` with the family's font-listing bounds.
pub fn fc_list(args: &[&str]) -> io::Result<Output> {
    FC_LIST.run(
        args.iter().copied(),
        FONT_LIST_STDOUT_LIMIT,
        HELPER_STDERR_LIMIT,
        FONT_HELPER_TIMEOUT,
    )
}

/// Run `fc-match` with the family's font-matching bounds.
pub fn fc_match(args: &[&str]) -> io::Result<Output> {
    FC_MATCH.run(
        args.iter().copied(),
        FONT_MATCH_STDOUT_LIMIT,
        HELPER_STDERR_LIMIT,
        FONT_HELPER_TIMEOUT,
    )
}

/// Run `notify-send` with the family's notification bounds.
pub fn notify_send(title: &str, body: &str) -> io::Result<Output> {
    // `--` keeps notification text beginning with `-` out of option parsing.
    NOTIFY_SEND.run(
        ["--", title, body],
        NOTIFICATION_OUTPUT_LIMIT,
        NOTIFICATION_OUTPUT_LIMIT,
        NOTIFICATION_HELPER_TIMEOUT,
    )
}

/// Resolve to the canonical target of one fixed absolute system candidate.
///
/// Canonicalising before exec closes the symlink-swap window at the original
/// pathname. The target and every directory above it must be owned by root
/// (or by this process's user) and not writable by group or other. A
/// non-root user's own owner-writable component is also refused for an
/// automatic helper; such a component is mutable application state, not a
/// system executable.
pub fn trusted_system_executable(candidate: &Path) -> Option<PathBuf> {
    trusted_system_executable_inner(candidate)
}

#[cfg(unix)]
fn trusted_system_executable_inner(candidate: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if !candidate.is_absolute() {
        return None;
    }
    let canonical = std::fs::canonicalize(candidate).ok()?;
    // SAFETY: geteuid has no preconditions and only reads process state.
    let euid = unsafe { libc::geteuid() };
    for (index, component) in canonical.ancestors().enumerate() {
        let metadata = std::fs::metadata(component).ok()?;
        let mode = metadata.permissions().mode();
        if index == 0 {
            if !metadata.is_file() || mode & 0o111 == 0 {
                return None;
            }
        } else if !metadata.is_dir() {
            return None;
        }
        if !trusted_component(mode, metadata.uid(), euid) {
            return None;
        }
    }
    Some(canonical)
}

#[cfg(unix)]
fn trusted_component(mode: u32, owner: u32, euid: u32) -> bool {
    if mode & 0o022 != 0 || (owner != 0 && owner != euid) {
        return false;
    }
    // Root can write every root-owned system file regardless of its mode, so
    // applying the ordinary self-writable rule to euid 0 would disable every
    // helper in containers. A non-root user's writable file is not an
    // automatic system helper, even when it occupies a fixed candidate path.
    euid == 0 || owner != euid || mode & 0o200 == 0
}

#[cfg(not(unix))]
fn trusted_system_executable_inner(_candidate: &Path) -> Option<PathBuf> {
    // Automatic helper integrations are Unix-only today. Keep other targets
    // fail-closed until they have an equivalent ownership policy.
    None
}

/// Capture both child streams under independent byte limits and one deadline.
///
/// The child leads a new process group owned by [`crate::supervised`]. Every
/// return path terminates that group and waits for the direct child,
/// including successful completion; a descendant cannot survive merely by
/// closing the inherited pipes early.
#[cfg(unix)]
pub fn bounded_command_output(
    command: &mut Command,
    stdout_limit: usize,
    stderr_limit: usize,
    timeout: Duration,
) -> io::Result<Output> {
    use std::os::fd::AsRawFd;
    use std::process::Stdio;
    use std::time::Instant;

    let deadline = Instant::now() + timeout;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::supervised::SupervisedChild::spawn(command)?;

    let mut stdout = match child.take_stdout() {
        Some(stdout) => stdout,
        None => {
            let _ = child.reap_after_group_kill();
            return Err(io::Error::other("helper stdout pipe was not created"));
        }
    };
    let mut stderr = match child.take_stderr() {
        Some(stderr) => stderr,
        None => {
            let _ = child.reap_after_group_kill();
            return Err(io::Error::other("helper stderr pipe was not created"));
        }
    };

    if let Err(error) =
        set_nonblocking(stdout.as_raw_fd()).and_then(|()| set_nonblocking(stderr.as_raw_fd()))
    {
        let _ = child.reap_after_group_kill();
        return Err(error);
    }

    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_closed = false;
    let mut stderr_closed = false;
    loop {
        if Instant::now() >= deadline {
            let _ = child.reap_after_group_kill();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "helper process exceeded its time limit",
            ));
        }

        let drained = drain_pipe(
            &mut stdout,
            &mut stdout_bytes,
            stdout_limit,
            &mut stdout_closed,
        )
        .and_then(|()| {
            drain_pipe(
                &mut stderr,
                &mut stderr_bytes,
                stderr_limit,
                &mut stderr_closed,
            )
        });
        if let Err(error) = drained {
            let _ = child.reap_after_group_kill();
            return Err(error);
        }

        // WNOWAIT observation keeps the leader waitable (and its PGID
        // reserved) until the group signal inside the reap.
        let exited = match child.root_has_exited() {
            Ok(exited) => exited,
            Err(error) => {
                let _ = child.reap_after_group_kill();
                return Err(error);
            }
        };
        if exited && stdout_closed && stderr_closed {
            let status = child.reap_after_group_kill()?;
            return Ok(Output {
                status,
                stdout: stdout_bytes,
                stderr: stderr_bytes,
            });
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_ms = remaining.as_millis().min(100).try_into().unwrap_or(100);
        let mut descriptors = Vec::with_capacity(2);
        if !stdout_closed {
            descriptors.push(libc::pollfd {
                fd: stdout.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            });
        }
        if !stderr_closed {
            descriptors.push(libc::pollfd {
                fd: stderr.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            });
        }
        // SAFETY: descriptors is live for the call, its length fits nfds_t,
        // and every fd it names is owned by this function.
        let polled = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len().try_into().unwrap_or(0),
                timeout_ms,
            )
        };
        if polled < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                let _ = child.reap_after_group_kill();
                return Err(error);
            }
        }
    }
}

#[cfg(not(unix))]
pub fn bounded_command_output(
    _command: &mut Command,
    _stdout_limit: usize,
    _stderr_limit: usize,
    _timeout: Duration,
) -> io::Result<Output> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "bounded app helpers are only supported on Unix",
    ))
}

#[cfg(unix)]
fn set_nonblocking(fd: std::os::fd::RawFd) -> io::Result<()> {
    // SAFETY: fd is a live descriptor owned by the caller; F_GETFL/F_SETFL
    // only query and update its file-status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above; the only change is adding O_NONBLOCK.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn drain_pipe(
    reader: &mut impl io::Read,
    output: &mut Vec<u8>,
    limit: usize,
    closed: &mut bool,
) -> io::Result<()> {
    if *closed {
        return Ok(());
    }
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                *closed = true;
                return Ok(());
            }
            Ok(read) => {
                if output.len().saturating_add(read) > limit {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("helper output exceeds the {limit} byte limit"),
                    ));
                }
                output.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(label: &str) -> Self {
            let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "jterm-core-helper-{label}-{}-{id}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir(&path).expect("create helper scratch directory");
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn automatic_helpers_are_canonical_absolute_system_programs() {
        assert!(trusted_system_executable(Path::new("fc-list")).is_none());

        let scratch = ScratchDir::new("untrusted-path");
        let fake = scratch.0.join("fc-list");
        std::fs::write(&fake, "#!/bin/sh\n").expect("write fake helper");
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
            .expect("make fake helper executable");
        assert!(
            trusted_system_executable(&fake).is_none(),
            "a helper below the world-writable temporary namespace is not trusted"
        );

        for helper in [&FC_LIST, &FC_MATCH, &NOTIFY_SEND] {
            if let Some(program) = helper.resolve() {
                assert!(program.is_absolute(), "{program:?}");
                assert_eq!(std::fs::canonicalize(&program).unwrap(), program);
            }
        }
    }

    #[test]
    fn trust_rejects_mutable_or_foreign_components_without_disabling_root() {
        const ROOT: u32 = 0;
        const USER: u32 = 1000;
        const OTHER: u32 = 2000;

        assert!(trusted_component(0o755, ROOT, USER));
        assert!(trusted_component(0o755, ROOT, ROOT));
        assert!(!trusted_component(0o775, ROOT, USER));
        assert!(!trusted_component(0o757, ROOT, USER));
        assert!(!trusted_component(0o755, USER, USER));
        assert!(trusted_component(0o555, USER, USER));
        assert!(!trusted_component(0o555, OTHER, USER));
    }

    #[test]
    fn stdout_and_stderr_are_drained_concurrently_under_independent_caps() {
        let script = "i=0; while [ \"$i\" -lt 4096 ]; do \
                      printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n'; \
                      printf 'yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy\n' >&2; \
                      i=$((i + 1)); done";
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);

        let output =
            bounded_command_output(&mut command, 256 * 1024, 256 * 1024, Duration::from_secs(5))
                .expect("both streams should drain without a pipe deadlock");

        assert!(output.status.success());
        assert!(output.stdout.len() > 128 * 1024);
        assert!(output.stderr.len() > 128 * 1024);
    }

    #[test]
    fn exceeding_either_stream_limit_fails_closed() {
        let mut stdout = Command::new("/bin/sh");
        stdout.args(["-c", "printf too-large"]);
        let error = bounded_command_output(&mut stdout, 4, 64, Duration::from_secs(2))
            .expect_err("stdout cap must be enforced");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let mut stderr = Command::new("/bin/sh");
        stderr.args(["-c", "printf too-large >&2"]);
        let error = bounded_command_output(&mut stderr, 64, 4, Duration::from_secs(2))
            .expect_err("stderr cap must be enforced");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn exit_observation_keeps_the_group_leader_waitable_until_cleanup() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 23"]);

        let output = bounded_command_output(&mut command, 64, 64, Duration::from_secs(2))
            .expect("observe and reap an immediately exiting helper");
        assert_eq!(output.status.code(), Some(23));
    }

    #[test]
    fn deadline_kills_descendants_and_reaps_the_direct_child() {
        let scratch = ScratchDir::new("deadline");
        let pid_file = scratch.0.join("leader-pid");
        let survivor_file = scratch.0.join("survived");
        let script = "printf '%s' \"$$\" > \"$1\"; \
                      (/bin/sleep 0.3; printf survived > \"$2\") & exit 0";
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(script)
            .arg("jterm-core-helper-test")
            .arg(&pid_file)
            .arg(&survivor_file);

        let started = Instant::now();
        let error = bounded_command_output(&mut command, 64, 64, Duration::from_millis(50))
            .expect_err("a descendant holding both pipes must meet the same deadline");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));

        let leader: libc::pid_t = std::fs::read_to_string(&pid_file)
            .expect("leader wrote its pid before exiting")
            .parse()
            .expect("numeric leader pid");
        let mut status = 0;
        // SAFETY: status is live and the bounded runner owned this child.
        let waited = unsafe { libc::waitpid(leader, &mut status, libc::WNOHANG) };
        assert_eq!(waited, -1, "the direct helper child was not reaped");
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );

        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !survivor_file.exists(),
            "a process in the helper group survived timeout cleanup"
        );
    }

    #[test]
    fn run_reports_name_for_missing_and_failing_helpers() {
        let missing = TrustedHelper::new(
            "jterm-core-no-such-helper",
            &["/nonexistent/jterm-core-no-such-helper"],
        );
        let error = missing
            .run(std::iter::empty::<&str>(), 64, 64, Duration::from_secs(1))
            .expect_err("an unresolved helper must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("jterm-core-no-such-helper"));

        let failing = TrustedHelper::new("sh", &["/bin/sh", "/usr/bin/sh"]);
        if failing.resolve().is_none() {
            // Non-standard development hosts may not have a system-owned sh.
            return;
        }
        let error = failing
            .run(["-c", "exit 7"], 64, 64, Duration::from_secs(2))
            .expect_err("a non-success exit must surface as an error");
        assert!(error.to_string().contains("sh exited unsuccessfully"));
    }
}
