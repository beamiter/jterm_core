//! Linux launch of an authenticated bundled Bash integration.

use crate::{
    shell_identity::{move_above_stdio, ShellIdentityChannel},
    startup_status::{self, StartupStage, StartupWriter},
};
use std::{
    ffi::{CString, OsString},
    fs::File,
    io::{self, Seek, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::Path,
    time::Duration,
};

pub(crate) struct BundledBash<'a> {
    pub(crate) executable: &'a Path,
    /// The caller supplies its embedded bundled startup script.
    pub(crate) startup_script: &'a [u8],
    pub(crate) cwd: &'a Path,
    pub(crate) environment: &'a [(OsString, OsString)],
    pub(crate) identity_fd_key: &'static str,
    pub(crate) identity_token_key: &'static str,
    pub(crate) rows: u16,
    pub(crate) columns: u16,
}

/// Owns the PTY and exact child wait until the runtime has bound the process.
pub(crate) struct PendingPty {
    master: OwnedFd,
    child: PendingChild,
    token: String,
}

impl PendingPty {
    /// Construct the transport without starting its workers in the callback.
    /// A failed adoption keeps child cleanup with the startup owner.
    pub(crate) fn adopt<T, E>(
        self,
        bind: impl FnOnce(OwnedFd, libc::pid_t, String) -> Result<T, E>,
    ) -> Result<T, E> {
        let Self {
            master,
            mut child,
            token,
        } = self;
        let owner = bind(master, child.0.expect("fresh child"), token)?;
        child.0 = None;
        Ok(owner)
    }
}

struct PendingChild(Option<libc::pid_t>);

impl Drop for PendingChild {
    fn drop(&mut self) {
        let Some(pid) = self.0.take() else { return };
        // SAFETY: the fresh PID is still owned and unreaped. Confirm it leads
        // its own group before signalling the group, including pre-setsid errors.
        unsafe {
            if libc::getpgid(pid) == pid {
                libc::kill(-pid, libc::SIGKILL);
            }
            libc::kill(pid, libc::SIGKILL);
            while libc::waitpid(pid, std::ptr::null_mut(), 0) < 0
                && *libc::__errno_location() == libc::EINTR
            {}
        }
    }
}

pub(crate) fn launch_bundled_bash(spec: BundledBash<'_>) -> io::Result<PendingPty> {
    if !matches!(
        spec.executable.as_os_str().as_bytes(),
        b"/bin/bash" | b"/usr/bin/bash"
    ) || spec.startup_script.is_empty()
        || !spec.cwd.is_absolute()
        || spec.rows == 0
        || spec.columns == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid bundled Bash launch",
        ));
    }
    require_exclusive_wait()?;
    let executable = CString::new(spec.executable.as_os_str().as_bytes())?;
    let script_file = sealed_bash_script(spec.startup_script)?;
    let script_fd = script_file.as_raw_fd();
    let script = CString::new(format!("/proc/self/fd/{script_fd}"))?;
    let cwd = CString::new(spec.cwd.as_os_str().as_bytes())?;
    let argv = [
        executable.as_ptr(),
        c"--noprofile".as_ptr(),
        c"--rcfile".as_ptr(),
        script.as_ptr(),
        c"-i".as_ptr(),
        std::ptr::null(),
    ];

    let (token, token_reader, token_writer) = ShellIdentityChannel::allocate()?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "shell identity entropy unavailable",
            )
        })?
        .into_parts();
    let environment = prepare_environment(&spec, token_reader.as_raw_fd())?;
    let envp = environment
        .iter()
        .map(|entry| entry.as_ptr())
        .chain([std::ptr::null()])
        .collect::<Vec<_>>();
    let (master, slave) = pty_pair(spec.rows, spec.columns)?;
    let (status_reader, status_writer) = startup_status::channel()?;
    let token_read_fd = token_reader.as_raw_fd();
    let token_write_fd = token_writer.as_raw_fd();

    // SAFETY: all Rust memory and descriptors needed by the child were
    // prepared before fork. The child branch calls only async-signal-safe
    // libc operations and does not return.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        unsafe {
            libc::close(master.as_raw_fd());
            libc::close(status_reader.as_raw_fd());
            libc::close(token_write_fd);
            attach_child_pty(slave.as_raw_fd(), &status_writer);
            if !clear_cloexec(token_read_fd) || !clear_cloexec(script_fd) {
                status_writer.report_and_exit(
                    StartupStage::ShellIdentity,
                    *libc::__errno_location(),
                    126,
                );
            }
            if libc::chdir(cwd.as_ptr()) < 0 {
                status_writer.report_and_exit(
                    StartupStage::WorkingDirectory,
                    *libc::__errno_location(),
                    126,
                );
            }
            libc::execve(executable.as_ptr(), argv.as_ptr(), envp.as_ptr());
            status_writer.report_and_exit(StartupStage::Exec, *libc::__errno_location(), 127);
        }
    }

    let child = PendingChild(Some(pid));
    drop(slave);
    drop(script_file);
    drop(status_writer);
    drop(token_reader);
    status_reader.wait(Duration::from_secs(15))?;
    let mut writer = File::from(token_writer);
    writer.write_all(token.as_bytes())?;
    writer.write_all(b"\n")?;
    drop(writer);
    Ok(PendingPty {
        master,
        child,
        token,
    })
}

fn sealed_bash_script(script: &[u8]) -> io::Result<OwnedFd> {
    // SAFETY: the name is NUL-terminated and the call returns a new descriptor.
    let fd = unsafe {
        libc::memfd_create(
            c"jterm-bundled-bash".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = move_above_stdio(unsafe { OwnedFd::from_raw_fd(fd) })?;
    let descriptor = fd.as_raw_fd();
    let mut file = File::from(fd);
    // Bash already opened the procfd path before interpreting this line.
    writeln!(file, "exec {descriptor}<&-")?;
    file.write_all(script)?;
    file.rewind()?;
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    // SAFETY: the file owns this memfd. Sealing prevents mutation before exec.
    if unsafe { libc::fcntl(descriptor, libc::F_ADD_SEALS, seals) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file.into())
}

fn prepare_environment(spec: &BundledBash<'_>, token_read_fd: RawFd) -> io::Result<Vec<CString>> {
    let mut environment = Vec::with_capacity(spec.environment.len() + 1);
    for (key, value) in spec.environment {
        let key = key.as_os_str().as_bytes();
        if key == spec.identity_fd_key.as_bytes() || key == spec.identity_token_key.as_bytes() {
            continue;
        }
        if key.is_empty() || key.contains(&b'=') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid shell environment key",
            ));
        }
        let mut entry = key.to_vec();
        entry.push(b'=');
        entry.extend_from_slice(value.as_os_str().as_bytes());
        environment.push(CString::new(entry)?);
    }
    environment.push(CString::new(format!(
        "{}={token_read_fd}",
        spec.identity_fd_key
    ))?);
    Ok(environment)
}

fn require_exclusive_wait() -> io::Result<()> {
    let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
    // SAFETY: the null new-action pointer requests the current SIGCHLD policy.
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), action.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let action = unsafe { action.assume_init() };
    if action.sa_sigaction != libc::SIG_DFL || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "authenticated Bash requires exclusive child wait ownership",
        ));
    }
    Ok(())
}

fn pty_pair(rows: u16, columns: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    // SAFETY: successful open calls return fresh descriptors for this process.
    let master = unsafe { libc::posix_openpt(flags | libc::O_NONBLOCK) };
    if master < 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let fd = master.as_raw_fd();
    // SAFETY: `master` remains live across these devpts operations.
    if unsafe { libc::grantpt(fd) } < 0 || unsafe { libc::unlockpt(fd) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut name = [0 as libc::c_char; 128];
    let name_error = unsafe { libc::ptsname_r(fd, name.as_mut_ptr(), name.len()) };
    if name_error != 0 {
        return Err(io::Error::from_raw_os_error(name_error));
    }
    let slave = unsafe { libc::open(name.as_ptr(), flags) };
    if slave < 0 {
        return Err(io::Error::last_os_error());
    }
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &winsize) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((master, slave))
}

/// # Safety
/// The forked child owns a live device and status writer.
unsafe fn attach_child_pty(slave: RawFd, status: &StartupWriter) {
    if libc::setsid() < 0 {
        status.report_and_exit(StartupStage::Session, *libc::__errno_location(), 126);
    }
    if libc::ioctl(slave, libc::TIOCSCTTY, 0) < 0 {
        status.report_and_exit(StartupStage::Terminal, *libc::__errno_location(), 126);
    }
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if libc::dup2(slave, fd) < 0 {
            status.report_and_exit(
                StartupStage::StandardStreams,
                *libc::__errno_location(),
                126,
            );
        }
        // dup2(fd, fd) leaves the CLOEXEC flag unchanged.
        if !clear_cloexec(fd) {
            status.report_and_exit(
                StartupStage::StandardStreams,
                *libc::__errno_location(),
                126,
            );
        }
    }
    if slave > libc::STDERR_FILENO {
        libc::close(slave);
    }
}

unsafe fn clear_cloexec(fd: RawFd) -> bool {
    let flags = libc::fcntl(fd, libc::F_GETFD);
    flags >= 0 && libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) >= 0
}
