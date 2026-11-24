//! A private Unix fork-to-exec status pipe. Child lifetime belongs to the spawner.

use crate::pty_reader::cloexec_pipe_above_stdio;
use std::ffi::CStr;
use std::fmt;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

const RECORD_LEN: usize = 2 * std::mem::size_of::<libc::c_int>();
const ENTERING_EXEC: [libc::c_int; 2] = [0, 0];

/// Capture the child thread's errno without formatting or allocating.
pub(crate) fn errno() -> libc::c_int {
    #[cfg(target_os = "linux")]
    // SAFETY: errno addresses the calling thread's libc storage.
    unsafe {
        *libc::__errno_location()
    }
    #[cfg(not(target_os = "linux"))]
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum StartupStage {
    Signals = b'S' as i32,
    Session = b'L' as i32,
    Terminal = b'T' as i32,
    StandardStreams = b'I' as i32,
    WorkingDirectory = b'C' as i32,
    ShellIdentity = b'F' as i32,
    Exec = b'E' as i32,
}

impl StartupStage {
    fn from_code(code: i32) -> Option<Self> {
        match u8::try_from(code).ok()? {
            b'S' => Some(Self::Signals),
            b'L' => Some(Self::Session),
            b'T' => Some(Self::Terminal),
            b'I' => Some(Self::StandardStreams),
            b'C' => Some(Self::WorkingDirectory),
            b'F' => Some(Self::ShellIdentity),
            b'E' => Some(Self::Exec),
            _ => None,
        }
    }
}

impl fmt::Display for StartupStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Signals => "configure child signals",
            Self::Session => "create child session",
            Self::Terminal => "attach controlling terminal",
            Self::StandardStreams => "connect child standard streams",
            Self::WorkingDirectory => "enter child working directory",
            Self::ShellIdentity => "inherit child descriptor",
            Self::Exec => "execute PTY program",
        })
    }
}

#[derive(Debug)]
pub enum StartupError {
    TimedOut {
        timeout: Duration,
        received: usize,
    },
    IncompleteRecord {
        received: usize,
    },
    ExecUnconfirmed,
    InvalidRecord,
    Pipe(io::Error),
    Child {
        stage: StartupStage,
        error: io::Error,
    },
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut { timeout, received } => write!(
                f,
                "shell fork-to-exec startup timed out after {} ms ({received}/{RECORD_LEN} status bytes)",
                timeout.as_millis(),
            ),
            Self::IncompleteRecord { received } => write!(
                f, "incomplete shell startup status ({received}/{RECORD_LEN} bytes)"
            ),
            Self::ExecUnconfirmed => f.write_str("PTY child closed the startup pipe before exec"),
            Self::InvalidRecord => f.write_str("invalid shell startup status record"),
            Self::Pipe(error) => write!(f, "shell startup status pipe failed: {error}"),
            Self::Child { stage, error } => write!(f, "could not {stage}: {error}"),
        }
    }
}

impl std::error::Error for StartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Pipe(error) | Self::Child { error, .. } => Some(error),
            _ => None,
        }
    }
}

impl From<StartupError> for io::Error {
    fn from(error: StartupError) -> Self {
        let kind = match &error {
            StartupError::TimedOut { .. } => io::ErrorKind::TimedOut,
            StartupError::IncompleteRecord { .. } | StartupError::InvalidRecord => {
                io::ErrorKind::InvalidData
            }
            StartupError::ExecUnconfirmed => io::ErrorKind::UnexpectedEof,
            StartupError::Pipe(error) | StartupError::Child { error, .. } => error.kind(),
        };
        Self::new(kind, error)
    }
}

pub struct StartupReader(OwnedFd);
pub struct StartupWriter(OwnedFd);

impl AsRawFd for StartupReader {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl AsRawFd for StartupWriter {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// Both ends are CLOEXEC and above stdio. Only the parent reader is nonblocking.
pub fn channel() -> io::Result<(StartupReader, StartupWriter)> {
    let (reader, writer) = cloexec_pipe_above_stdio()?;
    let fd = reader.as_raw_fd();
    // SAFETY: `reader` owns this live descriptor throughout both calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((StartupReader(reader), StartupWriter(writer)))
}

impl StartupReader {
    /// Drop the parent's writer before waiting. A complete entering-exec
    /// record followed by clean EOF is the exec barrier. Child setup failures
    /// report their stage before exiting, including execve errors.
    /// The spawner retains responsibility for observing or reaping the child.
    pub fn wait(self, timeout: Duration) -> Result<(), StartupError> {
        let started = Instant::now();
        let mut record = [0_u8; RECORD_LEN];
        let mut received = 0;
        let mut entering_exec = false;
        let fd = self.as_raw_fd();
        loop {
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return Err(StartupError::TimedOut { timeout, received });
            }

            let mut poll_fd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: ready flags are written to one valid pollfd.
            let ready = unsafe { libc::poll(&mut poll_fd, 1, round_up_ms(timeout - elapsed)) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(StartupError::Pipe(error));
            }
            if ready == 0 {
                continue;
            }

            let revents = poll_fd.revents;
            if revents & libc::POLLNVAL != 0
                || revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0
            {
                return Err(StartupError::Pipe(io::Error::other(
                    "invalid poll readiness",
                )));
            }

            // Data may be ready alongside HUP. Read first so a failure record
            // already in the pipe cannot be mistaken for empty EOF.
            let remaining = &mut record[received..];
            // SAFETY: the descriptor is live, and the slice covers the write.
            let read = unsafe { libc::read(fd, remaining.as_mut_ptr().cast(), remaining.len()) };
            if read > 0 {
                received += read as usize;
                if received == RECORD_LEN {
                    if record != [0; RECORD_LEN] || entering_exec {
                        return Err(decode_record(record));
                    }
                    entering_exec = true;
                    received = 0;
                }
            } else if read == 0 {
                if received != 0 {
                    return Err(StartupError::IncompleteRecord { received });
                }
                if revents & libc::POLLERR != 0 {
                    return Err(StartupError::Pipe(io::Error::other(
                        "poll reported an error",
                    )));
                }
                return if entering_exec {
                    Ok(())
                } else {
                    Err(StartupError::ExecUnconfirmed)
                };
            } else {
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock if revents & libc::POLLERR == 0 => continue,
                    _ => return Err(StartupError::Pipe(error)),
                }
            }
        }
    }
}

impl StartupWriter {
    /// Mark the exec transition, then run the prepared program or report its
    /// execve failure. This keeps the status record adjacent to the syscall.
    ///
    /// # Safety
    /// Call only in the forked PTY child. Both pointer slices must be
    /// null-terminated and point to live C strings up to their terminators.
    pub unsafe fn execve(
        &self,
        program: &CStr,
        argv: &[*const libc::c_char],
        envp: &[*const libc::c_char],
    ) -> ! {
        self.entering_exec();
        // SAFETY: the caller supplies execve's prepared string lists.
        unsafe { libc::execve(program.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
        self.report_and_exit(StartupStage::Exec, errno(), 127);
    }

    /// Mark the transition immediately before execve. EOF before this record
    /// can come from a child killed during setup, with no failure errno.
    pub(crate) fn entering_exec(&self) {
        if !self.write_record(ENTERING_EXEC) {
            // SAFETY: exit the forked child if its startup report was lost.
            unsafe { libc::_exit(126) }
        }
    }

    /// Report a captured errno and exit in the forked child without allocating,
    /// locking, or running Rust destructors. Capture errno before other calls.
    pub fn report_and_exit(&self, stage: StartupStage, child_errno: i32, exit_code: i32) -> ! {
        self.write_record([stage as libc::c_int, child_errno]);
        // SAFETY: exit this child without calling any atexit handlers.
        unsafe { libc::_exit(exit_code) }
    }

    fn write_record(&self, record: [libc::c_int; 2]) -> bool {
        let ptr = record.as_ptr().cast::<u8>();
        let fd = self.as_raw_fd();
        let mut offset = 0;
        while offset < RECORD_LEN {
            // SAFETY: `ptr` covers the remaining bytes of the fixed record.
            let written = unsafe { libc::write(fd, ptr.add(offset).cast(), RECORD_LEN - offset) };
            if written > 0 {
                offset += written as usize;
            } else if written == 0 || errno() != libc::EINTR {
                return false;
            }
        }
        true
    }
}

fn decode_record(record: [u8; RECORD_LEN]) -> StartupError {
    let (stage, errno) = record.split_at(std::mem::size_of::<libc::c_int>());
    let stage = StartupStage::from_code(libc::c_int::from_ne_bytes(stage.try_into().unwrap()));
    let errno = libc::c_int::from_ne_bytes(errno.try_into().unwrap());
    match stage {
        Some(stage) if errno > 0 => StartupError::Child {
            stage,
            error: io::Error::from_raw_os_error(errno),
        },
        _ => StartupError::InvalidRecord,
    }
}

fn round_up_ms(remaining: Duration) -> libc::c_int {
    let rounded =
        remaining.as_millis() + u128::from(!remaining.subsec_nanos().is_multiple_of(1_000_000));
    rounded.clamp(1, libc::c_int::MAX as u128) as libc::c_int
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ForkedChild(Option<libc::pid_t>);

    impl ForkedChild {
        fn reap(&mut self) -> i32 {
            let pid = self.0.expect("unreaped child");
            let mut status = 0;
            loop {
                // SAFETY: this is our forked child, and status is writable.
                if unsafe { libc::waitpid(pid, &mut status, 0) } == pid {
                    self.0 = None;
                    return status;
                }
                let error = io::Error::last_os_error();
                assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
            }
        }
    }

    impl Drop for ForkedChild {
        fn drop(&mut self) {
            if let Some(pid) = self.0 {
                // SAFETY: this is our unreaped child.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                self.reap();
            }
        }
    }

    fn fork_with(child: unsafe fn(&StartupWriter)) -> (StartupReader, ForkedChild) {
        let (reader, writer) = channel().unwrap();
        // SAFETY: the child callback uses only async-signal-safe syscalls.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "could not fork: {}", io::Error::last_os_error());
        if pid == 0 {
            unsafe {
                libc::close(reader.as_raw_fd());
                child(&writer);
                libc::_exit(0);
            }
        }
        drop(writer);
        (reader, ForkedChild(Some(pid)))
    }

    #[test]
    fn exec_eof_leaves_real_exit_status_for_the_owner() {
        let (reader, mut child) = fork_with(|writer| unsafe {
            let args = [
                c"sh".as_ptr(),
                c"-c".as_ptr(),
                c"exit 127".as_ptr(),
                std::ptr::null(),
            ];
            writer.entering_exec();
            libc::execv(c"/bin/sh".as_ptr(), args.as_ptr());
            writer.report_and_exit(StartupStage::Exec, errno(), 126);
        });
        reader.wait(Duration::from_secs(2)).unwrap();
        let status = child.reap();
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 127);
    }

    #[test]
    fn exec_failure_survives_poll_hangup_with_stage_and_errno() {
        let (reader, mut child) = fork_with(|writer| unsafe {
            let path = c"/dev/null/jterm-child";
            let args = [path.as_ptr(), std::ptr::null()];
            writer.entering_exec();
            libc::execv(path.as_ptr(), args.as_ptr());
            writer.report_and_exit(StartupStage::Exec, errno(), 127);
        });
        match reader.wait(Duration::from_secs(2)).unwrap_err() {
            StartupError::Child {
                stage: StartupStage::Exec,
                error,
            } => {
                assert_eq!(error.raw_os_error(), Some(libc::ENOTDIR));
            }
            error => panic!("expected exec failure, got {error}"),
        }
        assert_eq!(libc::WEXITSTATUS(child.reap()), 127);
    }

    #[test]
    fn child_death_during_setup_does_not_confirm_exec() {
        let (reader, mut child) = fork_with(|_| unsafe {
            libc::raise(libc::SIGKILL);
        });
        assert!(matches!(
            reader.wait(Duration::from_secs(2)),
            Err(StartupError::ExecUnconfirmed)
        ));
        assert_eq!(libc::WTERMSIG(child.reap()), libc::SIGKILL);
    }

    #[test]
    fn endpoints_are_above_stdio_and_only_reader_is_nonblocking() {
        let (reader, writer) = channel().unwrap();
        for (fd, nonblocking) in [(reader.as_raw_fd(), true), (writer.as_raw_fd(), false)] {
            assert!(fd > libc::STDERR_FILENO);
            // SAFETY: both wrappers stay alive for the descriptor probes.
            let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            let file_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            assert!(fd_flags >= 0 && fd_flags & libc::FD_CLOEXEC != 0);
            assert!(file_flags >= 0);
            assert_eq!(file_flags & libc::O_NONBLOCK != 0, nonblocking);
        }
    }
}
