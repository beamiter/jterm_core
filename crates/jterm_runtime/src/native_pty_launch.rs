//! Unix PTY allocation, fork-to-exec status, and exact child wait ownership.

#[cfg(all(target_os = "linux", feature = "linux-pty"))]
mod bundled_bash;
#[cfg(all(target_os = "linux", feature = "linux-pty"))]
pub(crate) use bundled_bash::{launch_bundled_bash, BundledBash};

pub use crate::startup_status::{StartupError, StartupStage};
use crate::{
    child_lifecycle::observe_child_exit,
    pty_instance_lock::lock_registration,
    pty_reader::move_above_stdio,
    startup_status::{self, StartupReader, StartupWriter},
    supervised::require_waitable_sigchld,
};
use std::{
    ffi::{CStr, CString},
    fmt, io,
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    time::Duration,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

pub enum WorkingDirectory<'a> {
    Inherit,
    Path(&'a CStr),
    Descriptor(BorrowedFd<'a>),
}

/// Product launch policy prepared in the parent, including argv[0].
pub struct PtyLaunch<'a> {
    pub executable: &'a CStr,
    pub argv: &'a [CString],
    pub environment: &'a [CString],
    pub directory: WorkingDirectory<'a>,
    pub rows: u16,
    pub columns: u16,
    /// Preserve a host's Linux SIGTERM policy when its launching thread dies.
    pub parent_death_signal: bool,
}

#[derive(Debug)]
pub enum PtyLaunchError {
    Prepare(io::Error),
    PrepareWorkingDirectory(io::Error),
    Start(StartupError),
    ExecConfirmed(io::Error),
}

impl PtyLaunchError {
    /// A retry in another directory is eligible only after a known cwd
    /// failure. A missing exec confirmation can also follow a completed exec.
    pub fn failed_working_directory(&self) -> bool {
        matches!(
            self,
            Self::PrepareWorkingDirectory(_)
                | Self::Start(StartupError::Child {
                    stage: StartupStage::WorkingDirectory,
                    ..
                })
        )
    }
}

impl fmt::Display for PtyLaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepare(error) => write!(f, "could not prepare PTY launch: {error}"),
            Self::PrepareWorkingDirectory(error) => {
                write!(f, "could not prepare PTY working directory: {error}")
            }
            Self::Start(error) => error.fmt(f),
            Self::ExecConfirmed(error) => {
                write!(f, "shell exec confirmed but setup failed: {error}")
            }
        }
    }
}

impl std::error::Error for PtyLaunchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Prepare(error)
            | Self::PrepareWorkingDirectory(error)
            | Self::ExecConfirmed(error) => Some(error),
            Self::Start(error) => Some(error),
        }
    }
}

impl From<io::Error> for PtyLaunchError {
    fn from(error: io::Error) -> Self {
        Self::Prepare(error)
    }
}

/// A launched PTY whose master and exact child wait still belong to startup.
pub struct PendingPty {
    master: OwnedFd,
    child: PendingChild,
}

impl PendingPty {
    /// Bind the existing transport without starting workers in the callback.
    /// Success transfers the exclusive child wait to that transport. On error
    /// the callback must leave the child alone so startup can clean it up.
    pub fn adopt<T, E>(
        self,
        bind: impl FnOnce(OwnedFd, libc::pid_t) -> Result<T, E>,
    ) -> Result<T, E> {
        let Self { master, mut child } = self;
        let owner = bind(master, child.0.expect("fresh child"))?;
        child.0 = None;
        Ok(owner)
    }

    fn confirm(self, reader: StartupReader, timeout: Duration) -> Result<Self, PtyLaunchError> {
        reader.wait(timeout).map_err(PtyLaunchError::Start)?;
        Ok(self)
    }
}

struct PendingChild(Option<libc::pid_t>);

impl Drop for PendingChild {
    fn drop(&mut self) {
        let Some(pid) = self.0.take() else { return };
        // WNOWAIT retains the identity until this exclusive owner has
        // finished signalling. A missing child never authorizes signals.
        loop {
            match observe_child_exit(pid, true) {
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
        // SAFETY: pid is still our unreaped child and we own its wait.
        unsafe {
            if libc::getpgid(pid) == pid {
                libc::kill(-pid, libc::SIGKILL);
            }
            libc::kill(pid, libc::SIGKILL);
            while libc::waitpid(pid, std::ptr::null_mut(), 0) < 0
                && startup_status::errno() == libc::EINTR
            {}
        }
    }
}

pub fn launch_pty(spec: PtyLaunch<'_>) -> Result<PendingPty, PtyLaunchError> {
    launch_with_inherited_fds(spec, &[], None)
}

fn launch_with_inherited_fds(
    spec: PtyLaunch<'_>,
    inherited: &[BorrowedFd<'_>],
    close_in_child: Option<BorrowedFd<'_>>,
) -> Result<PendingPty, PtyLaunchError> {
    if spec.argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "PTY argv is empty").into());
    }
    require_waitable_sigchld()?;
    let argv = exec_pointers(spec.argv);
    let envp = exec_pointers(spec.environment);
    // Pin the flock across fork and exclude concurrent registrations until
    // the descriptor snapshot belongs to the child as well as the parent.
    // Serializing through close also keeps concurrent launches from inheriting
    // one another's status writers before they are closed in the parent.
    let registration = lock_registration();
    let (master, slave) = pty_pair(spec.rows, spec.columns)?;
    let (status_reader, status_writer) = startup_status::channel()?;
    let instance_lock = registration.upgrade();
    // SAFETY: all memory needed in the child is prepared above. Its branch
    // touches no Rust locks or allocators and never returns to the caller.
    let parent = unsafe { libc::getpid() };
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if pid == 0 {
        unsafe {
            if let Some(lock) = &instance_lock {
                libc::close(lock.as_raw_fd());
            }
            libc::close(master.as_raw_fd());
            libc::close(status_reader.as_raw_fd());
            if let Some(fd) = close_in_child {
                libc::close(fd.as_raw_fd());
            }
            prepare_child_signals(parent, spec.parent_death_signal, &status_writer);
            attach_child_pty(slave.as_raw_fd(), spec.directory, &status_writer);
            for fd in inherited {
                let fd = fd.as_raw_fd();
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    child_failed(&status_writer, StartupStage::ShellIdentity);
                }
            }
            status_writer.entering_exec();
            libc::execve(spec.executable.as_ptr(), argv.as_ptr(), envp.as_ptr());
            child_failed(&status_writer, StartupStage::Exec);
        }
    }

    let pending = PendingPty {
        master,
        child: PendingChild(Some(pid)),
    };
    drop(slave);
    drop(status_writer);
    drop(instance_lock);
    drop(registration);
    pending.confirm(status_reader, STARTUP_TIMEOUT)
}

fn exec_pointers(entries: &[CString]) -> Vec<*const libc::c_char> {
    entries
        .iter()
        .map(|entry| entry.as_ptr())
        .chain([std::ptr::null()])
        .collect()
}

fn initial_geometry(rows: u16, columns: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

#[cfg(target_os = "linux")]
fn pty_pair(rows: u16, columns: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    // SAFETY: successful open calls return fresh descriptors for this process.
    let master = unsafe { libc::posix_openpt(flags | libc::O_NONBLOCK) };
    if master < 0 {
        return Err(io::Error::last_os_error());
    }
    let master = move_above_stdio(unsafe { OwnedFd::from_raw_fd(master) })?;
    let fd = master.as_raw_fd();
    // SAFETY: master pins fd across these devpts operations.
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
    let slave = move_above_stdio(unsafe { OwnedFd::from_raw_fd(slave) })?;
    let winsize = initial_geometry(rows, columns);
    if unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &winsize) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((master, slave))
}

#[cfg(not(target_os = "linux"))]
fn pty_pair(rows: u16, columns: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master = -1;
    let mut slave = -1;
    let winsize = initial_geometry(rows, columns);
    // SAFETY: openpty receives writable descriptor storage and a valid winsize.
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &winsize,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&master, &slave] {
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((move_above_stdio(master)?, move_above_stdio(slave)?))
}

fn child_failed(status: &StartupWriter, stage: StartupStage) -> ! {
    let exit_code = if stage == StartupStage::Exec {
        127
    } else {
        126
    };
    status.report_and_exit(stage, startup_status::errno(), exit_code)
}

/// # Safety
/// Called by the forked child with its own live status writer.
unsafe fn prepare_child_signals(
    parent: libc::pid_t,
    parent_death_signal: bool,
    status: &StartupWriter,
) {
    let mut action: libc::sigaction = std::mem::zeroed();
    action.sa_sigaction = libc::SIG_DFL;
    if libc::sigemptyset(&mut action.sa_mask) < 0 {
        child_failed(status, StartupStage::Signals);
    }
    let mut unblocked = action.sa_mask;
    for signal in [libc::SIGTERM, libc::SIGINT] {
        if libc::sigaction(signal, &action, std::ptr::null_mut()) < 0
            || libc::sigaddset(&mut unblocked, signal) < 0
        {
            child_failed(status, StartupStage::Signals);
        }
    }
    if libc::sigprocmask(libc::SIG_UNBLOCK, &unblocked, std::ptr::null_mut()) < 0 {
        child_failed(status, StartupStage::Signals);
    }

    #[cfg(target_os = "linux")]
    if parent_death_signal {
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) < 0 {
            child_failed(status, StartupStage::Signals);
        }
        // The kernel cannot deliver a notification for an earlier parent exit.
        if libc::getppid() != parent {
            status.report_and_exit(StartupStage::Signals, libc::ESRCH, 126);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (parent, parent_death_signal);
}

/// # Safety
/// The forked child owns a live slave above stdio and a private status writer.
unsafe fn attach_child_pty(slave: RawFd, directory: WorkingDirectory<'_>, status: &StartupWriter) {
    if libc::setsid() < 0 {
        child_failed(status, StartupStage::Session);
    }
    let changed_directory = match directory {
        WorkingDirectory::Inherit => 0,
        WorkingDirectory::Path(path) => libc::chdir(path.as_ptr()),
        WorkingDirectory::Descriptor(fd) => libc::fchdir(fd.as_raw_fd()),
    };
    if changed_directory < 0 {
        child_failed(status, StartupStage::WorkingDirectory);
    }
    if let WorkingDirectory::Descriptor(fd) = directory {
        if fd.as_raw_fd() > libc::STDERR_FILENO {
            libc::close(fd.as_raw_fd());
        }
    }
    if libc::ioctl(slave, libc::TIOCSCTTY, 0) < 0 {
        child_failed(status, StartupStage::Terminal);
    }
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if libc::dup2(slave, fd) < 0 {
            child_failed(status, StartupStage::StandardStreams);
        }
    }
    libc::close(slave);
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
