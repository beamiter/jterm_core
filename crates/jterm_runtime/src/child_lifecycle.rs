//! Unix child termination, process identity, and exclusive reaping.

use crate::process_stat::{process_stat, process_stat_result};
use std::ffi::c_int;
use std::io;
use std::num::NonZeroU8;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// How long the final SIGKILL drain keeps re-scanning a PTY session for
/// descendants that forked while the earlier signals were being delivered.
pub const SESSION_KILL_DRAIN_TIMEOUT: Duration = Duration::from_millis(150);

/// Delay between session re-scans inside [`SESSION_KILL_DRAIN_TIMEOUT`].
pub const SESSION_SCAN_INTERVAL: Duration = Duration::from_millis(10);

/// Consecutive empty scans required before a session counts as drained. One
/// empty scan is not enough: a descendant can fork between two scans, so the
/// drain only concludes after seeing the session empty twice in a row.
pub const REQUIRED_QUIET_SESSION_SCANS: u8 = 2;

/// What the lifecycle knows about a child that reached a terminal state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildExitOutcome {
    Exited(u8),
    Signaled(NonZeroU8),
    /// A foreign reaper supplied an already-converted code.
    ForeignCode(i32),
    /// This owner lost the wait status and retired the child identity.
    Unavailable,
}

impl ChildExitOutcome {
    /// The existing shell convention, including the fallback failure code.
    pub fn shell_code(self) -> i32 {
        match self {
            Self::Exited(code) => i32::from(code),
            Self::Signaled(signal) => 128 + i32::from(signal.get()),
            Self::ForeignCode(code) => code,
            Self::Unavailable => 1,
        }
    }

    /// Code for PTY consumers using negative signal numbers. A converted
    /// foreign code cannot distinguish a normal exit from a fatal signal.
    pub fn negative_signal_code(self) -> Option<i32> {
        match self {
            Self::Exited(code) => Some(i32::from(code)),
            Self::Signaled(signal) => Some(-i32::from(signal.get())),
            Self::ForeignCode(_) | Self::Unavailable => None,
        }
    }
}

/// Decode the terminal result of a successful `waitpid` or `wait4`. Statuses
/// for a stopped or continued child yield `None`.
pub fn decode_wait_status(status: c_int) -> Option<ChildExitOutcome> {
    if status < 0 {
        return None;
    }
    if libc::WIFEXITED(status) {
        u8::try_from(libc::WEXITSTATUS(status))
            .ok()
            .map(ChildExitOutcome::Exited)
    } else if libc::WIFSIGNALED(status) {
        u8::try_from(libc::WTERMSIG(status))
            .ok()
            .and_then(NonZeroU8::new)
            .map(ChildExitOutcome::Signaled)
    } else {
        None
    }
}

/// Interpret a raw `waitpid`/`wait4` status with the family's shell
/// convention: a normal exit yields its own code, and a fatal signal yields
/// `128 + signal` — the encoding [`crate::exit_status::signal_name_for_exit`]
/// decodes back into a name.
///
/// `None` means the status described a stop or continue rather than a
/// terminal state, so the child is still alive and must not be treated as
/// reaped. Frontends whose child watch is owned by another runtime (glib/vte
/// hands the raw status to its `child-exited` handler) use this to normalize
/// that status before [`ChildLifecycle::note_foreign_exit`].
pub fn exit_code_from_wait_status(status: c_int) -> Option<i32> {
    decode_wait_status(status).map(ChildExitOutcome::shell_code)
}

/// Observe a direct child's termination without releasing its wait status.
/// Nonblocking calls return `Ok(None)` when there is no exit to report.
/// The syscall runs once so callers can enforce their own EINTR deadline.
/// `ECHILD` means the status is unavailable to the caller.
pub fn observe_child_exit(pid: i32, nonblocking: bool) -> io::Result<Option<ChildExitOutcome>> {
    if pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid child PID",
        ));
    }
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let options = libc::WEXITED | libc::WNOWAIT | if nonblocking { libc::WNOHANG } else { 0 };
    // SAFETY: P_PID selects this direct child and info is writable. WNOWAIT
    // leaves the child waitable for its exclusive reaper.
    if unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, options) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: waitid succeeded. A zero PID means WNOHANG found no exit.
    let observed_pid = unsafe { info.si_pid() };
    if observed_pid == 0 {
        return Ok(None);
    }
    if observed_pid != pid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "waitid reported a different child",
        ));
    }
    // SAFETY: a matching positive PID from WEXITED provides a SIGCHLD status.
    let status = u8::try_from(unsafe { info.si_status() })
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid waitid status"))?;
    match info.si_code {
        libc::CLD_EXITED => Ok(Some(ChildExitOutcome::Exited(status))),
        libc::CLD_KILLED | libc::CLD_DUMPED => NonZeroU8::new(status)
            .map(|signal| Some(ChildExitOutcome::Signaled(signal)))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid waitid signal")),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected waitid exit reason",
        )),
    }
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: i32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open takes a pid and a flags word by value.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: pidfd_open returns a new owned descriptor on success.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }
}

#[cfg(target_os = "linux")]
fn send_pidfd_signal(pidfd: &OwnedFd, signal: c_int) -> io::Result<()> {
    // SAFETY: the descriptor is borrowed for the call and the siginfo pointer
    // is explicitly null, which asks the kernel to synthesize SI_USER.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// A handle to a running process that survives pid reuse.
///
/// A raw pid stops identifying anything the instant its process is reaped: the
/// kernel may hand the same number to an unrelated process before the next
/// `kill`. On Linux this type holds a pidfd, so every signal is bound to the
/// process that was opened and a reused number is rejected with `ESRCH`.
///
/// Where `pidfd_open` does not exist (non-Linux, or `ENOSYS`/`EPERM` from a
/// pre-5.3 kernel or a seccomp filter) the handle degrades to the raw pid and
/// [`ProcessRef::is_kernel_bound`] reports `false`. Callers that need the
/// stronger guarantee must check it rather than assume it.
#[derive(Debug)]
pub struct ProcessRef {
    pid: i32,
    #[cfg(target_os = "linux")]
    pidfd: Option<OwnedFd>,
    /// Mirrors `pidfd.is_some()` so platform-independent code can branch
    /// without repeating the cfg.
    kernel_bound: bool,
}

impl ProcessRef {
    /// Open a reuse-proof reference to `pid`.
    ///
    /// Fails when `pid` is not positive, or when the process cannot be
    /// referenced at all — most importantly `ESRCH`, which means the pid is
    /// already gone and any signal sent to that number would land on whatever
    /// the kernel assigns next. Only "pidfd is unavailable here" errors
    /// (`ENOSYS`, `EPERM`) degrade to the raw-pid fallback.
    pub fn open(pid: i32) -> io::Result<Self> {
        if pid <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process id must be positive",
            ));
        }
        #[cfg(target_os = "linux")]
        {
            match open_pidfd(pid) {
                Ok(pidfd) => Ok(Self {
                    pid,
                    pidfd: Some(pidfd),
                    kernel_bound: true,
                }),
                Err(error)
                    if matches!(error.raw_os_error(), Some(libc::ENOSYS) | Some(libc::EPERM)) =>
                {
                    Ok(Self::unverified(pid))
                }
                Err(error) => Err(error),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(Self::unverified(pid))
        }
    }

    /// The raw-pid fallback. Deliberately private: a public constructor taking
    /// a bare pid would be an unverified `kill(pid, …)` one call away, which is
    /// precisely the hazard this type exists to remove.
    fn unverified(pid: i32) -> Self {
        Self {
            pid,
            #[cfg(target_os = "linux")]
            pidfd: None,
            kernel_bound: false,
        }
    }

    /// The numeric pid. Safe to display or to pass to `/proc` probes; never
    /// signal it directly — use [`ProcessRef::signal`].
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Whether signals through this handle are bound to a kernel object and
    /// therefore immune to pid reuse.
    pub fn is_kernel_bound(&self) -> bool {
        self.kernel_bound
    }

    /// Send `signal` to exactly this process.
    ///
    /// Kernel-bound handles cannot reach a reused pid. Fallback handles carry
    /// the same reuse exposure as a plain `kill(pid, …)`, so callers holding
    /// one are responsible for the pid still being theirs — which is what
    /// [`ChildLifecycle`] guarantees by never releasing a pid while a signal
    /// may be in flight.
    pub fn signal(&self, signal: c_int) -> io::Result<()> {
        if self.pid <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process id must be positive",
            ));
        }
        #[cfg(target_os = "linux")]
        if let Some(pidfd) = self.pidfd.as_ref() {
            return send_pidfd_signal(pidfd, signal);
        }
        // SAFETY: kill takes a pid and a signal number by value.
        let result = unsafe { libc::kill(self.pid, signal) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Send `signal` to this process' whole group, but only after `/proc`
    /// confirms the process still leads that group.
    ///
    /// The check is what makes the negative-pid `kill` safe to expose: without
    /// it, `kill(-pid, …)` on a reused or non-leader pid would broadcast to an
    /// unrelated group. Returns `InvalidInput` when the process does not lead
    /// its group (including when `/proc` cannot answer).
    pub fn signal_process_group(&self, signal: c_int) -> io::Result<()> {
        if process_stat(self.pid).is_none_or(|stat| stat.process_group != self.pid) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process does not lead its own process group",
            ));
        }
        // SAFETY: a negative pid addresses the process group of its absolute
        // value, which the check above proved this process leads.
        let result = unsafe { libc::kill(-self.pid, signal) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// Who calls `waitpid` for a child.
///
/// This is the single most consequential field on a [`ChildLifecycle`]:
/// `waitpid` is destructive, so exactly one owner in the process may call it
/// for a given pid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReapOwner {
    /// This lifecycle reaps the child. It calls `waitpid`, caches the status,
    /// and its `Drop` guarantees the child is never left a zombie.
    Ours,
    /// Another runtime in this process reaps the child — a glib/vte pane
    /// watch, or a `std::process::Child` the caller still owns.
    ///
    /// A `Foreign` lifecycle **never** calls `waitpid`: doing so would consume
    /// the status the real owner is waiting for, and would release the pid for
    /// reuse while that owner still believes it holds the child. It still
    /// signals safely through [`ProcessRef`] and still runs the escalation
    /// ladder; it simply learns the exit status via
    /// [`ChildLifecycle::note_foreign_exit`] instead of taking it.
    Foreign,
}

/// The SIGHUP → SIGTERM → SIGKILL ladder, its timings, and whether the final
/// SIGKILL sweeps the child's whole PTY session.
///
/// Use [`EscalationPolicy::SESSION_DRAIN`] for a PTY whose child is a session
/// leader, and [`EscalationPolicy::PROCESS_GROUP`] when the caller only owns a
/// process group and must not enumerate `/proc`.
#[derive(Clone, Copy, Debug)]
pub struct EscalationPolicy {
    /// First, polite signal — a PTY hangup the shell can act on.
    pub hangup_signal: c_int,
    /// Grace period before escalating past `hangup_signal`.
    pub term_delay: Duration,
    /// Second signal, still catchable.
    pub term_signal: c_int,
    /// Grace period before the uncatchable signal.
    pub kill_delay: Duration,
    /// Final, uncatchable signal.
    pub kill_signal: c_int,
    /// Upper bound on the final drain (see [`SESSION_KILL_DRAIN_TIMEOUT`]).
    pub drain_timeout: Duration,
    /// Delay between drain scans (see [`SESSION_SCAN_INTERVAL`]).
    pub scan_interval: Duration,
    /// Empty scans required to declare quiescence (see
    /// [`REQUIRED_QUIET_SESSION_SCANS`]).
    pub required_quiet_scans: u8,
    /// Poll interval while waiting for the child to become reapable.
    pub reap_poll_interval: Duration,
    /// Whether the ladder enumerates `/proc` for every member of the child's
    /// session.
    ///
    /// Shell job control puts each foreground command in its own process
    /// group, so a single `kill(-pgid, …)` misses stubborn background jobs
    /// that a `setsid` PTY child still owns. Enumerating the session reaches
    /// them. Callers that own only a process group set this `false` and get
    /// the same ladder without the `/proc` sweep.
    pub drain_session: bool,
}

impl EscalationPolicy {
    const fn ladder(drain_session: bool) -> Self {
        Self {
            hangup_signal: libc::SIGHUP,
            term_delay: Duration::from_millis(120),
            term_signal: libc::SIGTERM,
            kill_delay: Duration::from_millis(250),
            kill_signal: libc::SIGKILL,
            drain_timeout: SESSION_KILL_DRAIN_TIMEOUT,
            scan_interval: SESSION_SCAN_INTERVAL,
            required_quiet_scans: REQUIRED_QUIET_SESSION_SCANS,
            reap_poll_interval: Duration::from_millis(25),
            drain_session,
        }
    }

    /// The full PTY behavior: HUP → TERM → KILL, with the final KILL repeated
    /// across the child's whole session until two consecutive scans find it
    /// empty.
    pub const SESSION_DRAIN: Self = Self::ladder(true);

    /// The same ladder restricted to the child and its process group. No
    /// `/proc` enumeration and no quiet-scan loop, for callers that do not own
    /// a session.
    pub const PROCESS_GROUP: Self = Self::ladder(false);
}

impl Default for EscalationPolicy {
    fn default() -> Self {
        Self::SESSION_DRAIN
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct SessionScan {
    live_members: usize,
    failed_members: usize,
    complete: bool,
}

/// Signal every live process whose session is `session_leader`, excluding the
/// leader itself, and report what the sweep saw.
fn signal_session_members(session_leader: i32, signal: c_int) -> SessionScan {
    let mut scan = SessionScan::default();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return scan;
    };
    scan.complete = true;

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                scan.complete = false;
                continue;
            }
        };
        let Some(member) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if member == session_leader {
            continue;
        }
        let observed = match process_stat_result(member) {
            Ok(observed) => observed,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                scan.complete = false;
                continue;
            }
        };
        if observed.session != session_leader || !observed.is_live() {
            continue;
        }

        #[cfg(target_os = "linux")]
        {
            // Open the stable process reference before the authoritative second
            // stat read. If the numeric pid was reused at either boundary, the
            // second read will describe a different session and we skip it;
            // pidfd_send_signal can therefore never target that replacement.
            //
            // `open_pidfd` is used directly rather than `ProcessRef::open`:
            // this path must NOT take the ENOSYS/EPERM raw-pid fallback, since
            // these pids were discovered by scanning /proc and are not owned by
            // this process at all.
            match open_pidfd(member) {
                Ok(pidfd) => {
                    let current = match process_stat_result(member) {
                        Ok(current) => current,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(_) => {
                            scan.complete = false;
                            scan.failed_members += 1;
                            continue;
                        }
                    };
                    if current.session != session_leader || !current.is_live() {
                        continue;
                    }
                    scan.live_members += 1;
                    if let Err(error) = send_pidfd_signal(&pidfd, signal) {
                        if error.raw_os_error() != Some(libc::ESRCH) {
                            scan.failed_members += 1;
                        }
                    }
                }
                Err(_) => {
                    // Count a still-live member so the bounded final drain keeps
                    // retrying. Never fall back to kill(raw_pid): doing so would
                    // recreate the PID-reuse race this path exists to prevent.
                    match process_stat_result(member) {
                        Ok(current) if current.session == session_leader && current.is_live() => {
                            scan.live_members += 1;
                            scan.failed_members += 1;
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(_) => {
                            scan.complete = false;
                            scan.failed_members += 1;
                        }
                        Ok(_) => {}
                    }
                }
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            scan.live_members += 1;
            // SAFETY: kill takes a pid and a signal number by value.
            let result = unsafe { libc::kill(member, signal) };
            if result != 0 {
                scan.failed_members += 1;
            }
        }
    }
    scan
}

/// Deliver `signal` to a child and everything it dragged along: its session
/// (when `drain_session` and the child leads one), otherwise its process
/// group, and always the child itself through the verified [`ProcessRef`].
///
/// Private on purpose. The direct-hit step is the one place a raw pid could
/// leak into a `kill`, and routing it through `ProcessRef` is what keeps that
/// impossible for callers.
fn signal_process_tree(process: &ProcessRef, signal: c_int, drain_session: bool) -> SessionScan {
    let pid = process.pid();
    if pid <= 0 {
        return SessionScan::default();
    }

    let scan = if drain_session && process_stat(pid).is_some_and(|stat| stat.session == pid) {
        signal_session_members(pid, signal)
    } else {
        // Not a session leader (or session drain disabled): the group is the
        // widest safe target. `signal_process_group` verifies leadership.
        let _ = process.signal_process_group(signal);
        SessionScan::default()
    };
    // Always hit the child itself: a shell that has not yet called
    // setsid/setpgid appears in neither sweep above.
    let _ = process.signal(signal);
    scan
}

fn repeat_session_scans_until_quiet<Scan>(
    deadline: Instant,
    interval: Duration,
    required_quiet_scans: u8,
    mut scan: Scan,
) -> bool
where
    Scan: FnMut() -> usize,
{
    let mut quiet_scans = 0u8;
    loop {
        if Instant::now() >= deadline {
            return false;
        }
        if scan() == 0 {
            quiet_scans = quiet_scans.saturating_add(1);
            if quiet_scans >= required_quiet_scans {
                return true;
            }
        } else {
            quiet_scans = 0;
        }

        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(interval.min(deadline.saturating_duration_since(now)));
    }
}

/// Signal the tree, then — for a session leader under a session-draining
/// policy — keep re-signaling until two consecutive scans find the session
/// empty or `policy.drain_timeout` expires.
fn signal_process_tree_until_quiet(process: &ProcessRef, signal: c_int, policy: &EscalationPolicy) {
    let pid = process.pid();
    if pid <= 0 {
        return;
    }
    let is_session_leader =
        policy.drain_session && process_stat(pid).is_some_and(|stat| stat.session == pid);
    let deadline = Instant::now() + policy.drain_timeout;
    let initial = signal_process_tree(process, signal, policy.drain_session);
    if !is_session_leader {
        return;
    }

    let mut remaining = initial.live_members;
    let mut failures = initial.failed_members;
    let mut scan_complete = initial.complete;
    let quiet = repeat_session_scans_until_quiet(
        deadline,
        policy.scan_interval,
        policy.required_quiet_scans,
        || {
            let current = signal_session_members(pid, signal);
            remaining = current.live_members;
            failures = current.failed_members;
            scan_complete = current.complete;
            if current.complete {
                current.live_members
            } else {
                usize::MAX
            }
        },
    );
    if !quiet {
        log::warn!(
            "session {pid} did not reach verified quiescence after the final signal drain \
             ({remaining} live member(s), {failures} safe-signal failure(s), scan complete: {scan_complete})"
        );
    }
}

#[derive(Default)]
struct ChildState {
    exit_outcome: Option<ChildExitOutcome>,
    termination_started: bool,
    cleanup_complete: bool,
}

/// Serializes child reaping with every signal sent during shutdown.
///
/// A raw pid becomes unsafe the instant `waitpid` releases it: the kernel may
/// immediately assign it to an unrelated process. Keeping both operations
/// behind one mutex means no cleanup worker can signal between a successful
/// reap and the corresponding state update — and on Linux the signal is bound
/// to a pidfd on top of that.
///
/// Three properties matter to callers:
/// - **Cached status.** Once the child is reaped its code is remembered, so
///   later drop paths never call `waitpid` a second time on a released pid.
/// - **One escalation, ever.** [`ChildLifecycle::terminate`] returns `false`
///   if termination already began, so two shutdown paths racing on the same
///   child cannot start two escalation threads.
/// - **No leaked zombie.** For [`ReapOwner::Ours`], `Drop` reaps a child that
///   was never terminated, killing it first if necessary. For
///   [`ReapOwner::Foreign`] the real owner is left undisturbed.
pub struct ChildLifecycle {
    process: ProcessRef,
    owner: ReapOwner,
    state: Mutex<ChildState>,
}

impl ChildLifecycle {
    /// Take ownership of the termination path for `pid`.
    ///
    /// `owner` states who calls `waitpid`; read [`ReapOwner`] before choosing,
    /// because getting it wrong either steals another runtime's exit status or
    /// leaks a zombie.
    ///
    /// Fails when `pid` is not positive, or when the process cannot be
    /// referenced at all (`ESRCH` — it is already gone, so its number may
    /// already belong to somebody else). A merely unavailable `pidfd_open`
    /// degrades to the raw-pid fallback instead, reported by
    /// [`ChildLifecycle::is_kernel_bound`].
    pub fn new(pid: i32, owner: ReapOwner) -> io::Result<Arc<Self>> {
        // A non-positive pid must never reach waitpid: 0, -1 and negatives all
        // mean "some other set of children" there, not "this child".
        let process = ProcessRef::open(pid)?;
        Ok(Self::with_process(process, owner))
    }

    #[cfg(all(target_os = "linux", feature = "linux-pty"))]
    pub(crate) fn new_kernel_bound(pid: i32) -> io::Result<Arc<Self>> {
        let process = ProcessRef::open(pid)?;
        if !process.is_kernel_bound() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "kernel-bound child reference unavailable",
            ));
        }
        Ok(Self::with_process(process, ReapOwner::Ours))
    }

    fn with_process(process: ProcessRef, owner: ReapOwner) -> Arc<Self> {
        Arc::new(Self {
            process,
            owner,
            state: Mutex::new(ChildState::default()),
        })
    }

    /// The child's pid, for logging and `/proc` probes.
    pub fn pid(&self) -> i32 {
        self.process.pid()
    }

    /// Who reaps this child.
    pub fn owner(&self) -> ReapOwner {
        self.owner
    }

    /// Whether signals to this child are immune to pid reuse.
    pub fn is_kernel_bound(&self) -> bool {
        self.process.is_kernel_bound()
    }

    /// The cached exit code, if the status has already been observed. Never
    /// calls `waitpid`, so it is safe on any thread and after any drop path.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_outcome().map(ChildExitOutcome::shell_code)
    }

    /// Cached terminal outcome. `Unavailable` records a lost wait status.
    /// Calling this method never observes or consumes a kernel status.
    pub fn exit_outcome(&self) -> Option<ChildExitOutcome> {
        self.state().exit_outcome
    }

    fn state(&self) -> MutexGuard<'_, ChildState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            log::error!("child lifecycle lock was poisoned; recovering its state");
            poisoned.into_inner()
        })
    }

    fn poll_reap_locked(&self, state: &mut ChildState) -> Option<ChildExitOutcome> {
        if let Some(outcome) = state.exit_outcome {
            return Some(outcome);
        }
        if self.owner == ReapOwner::Foreign {
            // Another runtime is blocked in waitpid on this pid. Reaping here
            // would consume the status it is waiting for and release the pid
            // for reuse behind its back.
            return None;
        }
        // During explicit shutdown the leader intentionally remains a zombie
        // until HUP → TERM → KILL has been delivered to its whole PTY session.
        // Retaining the pid prevents the kernel from reusing the session id
        // while the cleanup worker is still signaling descendants.
        if state.termination_started && !state.cleanup_complete {
            return None;
        }

        // Natural exits need group cleanup too. Observe without consuming the
        // status so the zombie pins its numeric PID/PGID through the last
        // signal. Reaping first would leave HUP-resistant background jobs
        // alive and make any later group signal vulnerable to PID reuse.
        if !state.cleanup_complete {
            // The state lock holds the exclusive reaper's ownership while
            // observing and using the still-unreaped child's group number.
            match observe_child_exit(self.process.pid(), true) {
                Ok(Some(_)) => {}
                Ok(None) => return None,
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                    let outcome = ChildExitOutcome::Unavailable;
                    state.exit_outcome = Some(outcome);
                    return Some(outcome);
                }
                Err(error) => {
                    if error.raw_os_error() != Some(libc::EINTR) {
                        log::warn!("failed to observe child {}: {error}", self.process.pid());
                    }
                    return None;
                }
            }
            // The unreaped direct child reserves this group number. If it
            // never created a private group, kill(-pid) simply returns ESRCH;
            // it cannot address our own or an unrelated process group.
            let killed = unsafe { libc::kill(-self.process.pid(), libc::SIGKILL) };
            if killed < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    // Keep the ownership anchor and retry on a later poll.
                    log::warn!(
                        "failed to clean exited child {} group: {error}",
                        self.process.pid()
                    );
                    return None;
                }
            }
        }

        let mut status: c_int = 0;
        // SAFETY: `status` is a live c_int for the duration of the call.
        let waited = unsafe { libc::waitpid(self.process.pid(), &mut status, libc::WNOHANG) };
        if waited > 0 {
            // A terminal status; a stop/continue report leaves the child alive.
            let outcome = decode_wait_status(status)?;
            state.exit_outcome = Some(outcome);
            return Some(outcome);
        }
        if waited == 0 {
            // WNOHANG and no state change: still running.
            return None;
        }

        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ECHILD) => {
                // Another in-process owner should not reap this child, but if
                // it did, the pid is no longer ours and must never be signaled.
                let outcome = ChildExitOutcome::Unavailable;
                state.exit_outcome = Some(outcome);
                Some(outcome)
            }
            Some(libc::EINTR) => None,
            _ => {
                log::warn!("failed to poll child {}: {error}", self.process.pid());
                None
            }
        }
    }

    /// Reap the child without blocking, returning its exit code once known.
    ///
    /// Always `None` for [`ReapOwner::Foreign`] until
    /// [`ChildLifecycle::note_foreign_exit`] supplies the status.
    pub fn poll_reap(&self) -> Option<i32> {
        self.poll_reap_outcome().map(ChildExitOutcome::shell_code)
    }

    /// Reap the child without blocking and preserve the origin of the status.
    pub fn poll_reap_outcome(&self) -> Option<ChildExitOutcome> {
        let mut state = self.state();
        self.poll_reap_locked(&mut state)
    }

    /// Record the exit code a foreign reaper observed (see
    /// [`exit_code_from_wait_status`] to normalize a raw `waitpid` status).
    ///
    /// This retires the pid: after it, the lifecycle reports the status from
    /// its cache and refuses to send any further signal, because the number is
    /// now free for reuse. Ignored — with a log line — for
    /// [`ReapOwner::Ours`], whose status comes from its own `waitpid`.
    pub fn note_foreign_exit(&self, exit_code: i32) {
        if self.owner != ReapOwner::Foreign {
            log::error!(
                "ignoring a foreign exit status for child {}, which this process reaps itself",
                self.process.pid()
            );
            return;
        }
        let mut state = self.state();
        if state.exit_outcome.is_some() {
            return;
        }
        state.exit_outcome = Some(ChildExitOutcome::ForeignCode(exit_code));
        state.cleanup_complete = true;
    }

    /// Send `signal` to the child, refusing once its status has been observed.
    ///
    /// The refusal is the point: after the status is known the pid may already
    /// belong to another process.
    pub fn signal(&self, signal: c_int) -> io::Result<()> {
        let state = self.state();
        if state.exit_outcome.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "child has already been reaped",
            ));
        }
        self.process.signal(signal)
    }

    /// Send `signal` to the child's process group, refusing once its status
    /// has been observed and unless `/proc` confirms it leads that group.
    pub fn signal_process_group(&self, signal: c_int) -> io::Result<()> {
        let state = self.state();
        if state.exit_outcome.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "child has already been reaped",
            ));
        }
        self.process.signal_process_group(signal)
    }

    fn signal_during_cleanup(&self, signal: c_int, policy: &EscalationPolicy) -> bool {
        // The guard is held across the signal: that is the invariant.
        let state = self.state();
        if state.exit_outcome.is_some() {
            return false;
        }
        signal_process_tree(&self.process, signal, policy.drain_session);
        true
    }

    fn kill_during_cleanup(&self, policy: &EscalationPolicy) -> bool {
        let state = self.state();
        if state.exit_outcome.is_some() {
            return false;
        }
        signal_process_tree_until_quiet(&self.process, policy.kill_signal, policy);
        true
    }

    fn begin_termination(&self, policy: &EscalationPolicy) -> bool {
        let mut state = self.state();
        if state.exit_outcome.is_some() || state.termination_started {
            return false;
        }
        state.termination_started = true;
        signal_process_tree(&self.process, policy.hangup_signal, policy.drain_session);
        true
    }

    fn finish_termination_and_reap(&self, policy: &EscalationPolicy) {
        {
            let mut state = self.state();
            state.cleanup_complete = true;
        }
        if self.owner == ReapOwner::Foreign {
            // The foreign owner's watch delivers the status; polling here
            // would either spin forever or steal it.
            return;
        }
        while self.poll_reap().is_none() {
            std::thread::sleep(policy.reap_poll_interval);
        }
    }

    /// Start the escalation ladder on a background thread and return `true`.
    ///
    /// Returns `false` — doing nothing — when the child was already reaped or
    /// termination already began. That makes a second call from a racing
    /// shutdown path (a widget drop concurrent with an explicit close, say)
    /// harmless: exactly one escalation thread can ever exist per child.
    ///
    /// The ladder runs off the caller's thread so a UI main loop is never
    /// blocked by the grace periods. For [`ReapOwner::Ours`] the worker holds
    /// an `Arc` until the child is reaped, so the lifecycle outlives the
    /// caller's handle and `Drop` finds nothing left to do.
    pub fn terminate(self: &Arc<Self>, policy: EscalationPolicy) -> bool {
        if !self.begin_termination(&policy) {
            return false;
        }
        let worker_child = Arc::clone(self);
        if let Err(error) = std::thread::Builder::new()
            .name("jterm-process-cleanup".to_string())
            .spawn(move || {
                std::thread::sleep(policy.term_delay);
                worker_child.signal_during_cleanup(policy.term_signal, &policy);
                std::thread::sleep(policy.kill_delay);
                worker_child.kill_during_cleanup(&policy);
                worker_child.finish_termination_and_reap(&policy);
            })
        {
            log::error!("failed to start the process cleanup worker: {error}");
            self.force_kill_and_reap_with(&policy);
        }
        true
    }

    /// Kill the child now and, for [`ReapOwner::Ours`], block until it is
    /// reaped — skipping the ladder's grace periods entirely.
    ///
    /// Intended for the paths where no concurrent reader exists to observe the
    /// exit (a reader thread that could not be spawned, an aborted setup), and
    /// used by `Drop` for a child that was never terminated. Every later drop
    /// path becomes a no-op because the status is cached.
    ///
    /// For [`ReapOwner::Foreign`] this kills and drains but does not wait; the
    /// foreign owner still delivers the status.
    pub fn force_kill_and_reap(&self) {
        self.force_kill_and_reap_with(&EscalationPolicy::SESSION_DRAIN);
    }

    fn force_kill_and_reap_with(&self, policy: &EscalationPolicy) {
        let mut state = self.state();
        if state.exit_outcome.is_some() {
            return;
        }
        state.termination_started = true;
        signal_process_tree_until_quiet(&self.process, policy.kill_signal, policy);
        if self.owner == ReapOwner::Foreign {
            state.cleanup_complete = true;
            return;
        }
        state.exit_outcome = Some(blocking_reap(self.process.pid()));
        state.cleanup_complete = true;
    }
}

impl Drop for ChildLifecycle {
    fn drop(&mut self) {
        if self.owner == ReapOwner::Foreign {
            // Not ours to wait for, and not ours to kill on the way out.
            return;
        }
        // Normally the escalation worker already reaped the child and this is
        // a cached lookup. Otherwise the child was never terminated, and
        // letting it go would leak a zombie for the process' lifetime.
        if self.poll_reap().is_some() {
            return;
        }
        self.force_kill_and_reap_with(&EscalationPolicy::SESSION_DRAIN);
    }
}

/// Block until `pid` yields a terminal status, retrying through `EINTR`.
///
/// An unexpected failure, such as an outside reaper consuming the status,
/// produces an unavailable outcome.
fn blocking_reap(pid: i32) -> ChildExitOutcome {
    loop {
        let mut status: c_int = 0;
        // SAFETY: `status` is a live c_int for the duration of the call.
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        if waited > 0 {
            if let Some(outcome) = decode_wait_status(status) {
                return outcome;
            }
            // A stop/continue report; keep waiting for the terminal status.
            continue;
        }
        if waited == 0 {
            // POSIX only allows 0 with WNOHANG, which is not set here.
            continue;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        log::warn!("failed to reap child {pid}: {error}");
        return ChildExitOutcome::Unavailable;
    }
}

/// Lifecycle tests spawn real short-lived processes: the invariants under test
/// (pid reuse, session drains, who owns `waitpid`) exist only against a real
/// kernel and cannot be observed through a fake.
#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::exit_status::signal_name_for_exit;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    /// A child that outlives the test only if the test fails.
    #[allow(clippy::zombie_processes)] // Each test decides who reaps.
    fn spawn_sleeper() -> Child {
        Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .expect("spawn test child")
    }

    fn wait_for<Ready: FnMut() -> bool>(timeout: Duration, mut ready: Ready) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if ready() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// True when `pid` is not a reapable child of this process any more.
    fn is_fully_reaped(pid: i32) -> bool {
        let mut status: c_int = 0;
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        waited < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
    }

    #[cfg(target_os = "linux")]
    fn pidfd_is_unavailable(error: &io::Error) -> bool {
        matches!(error.raw_os_error(), Some(libc::ENOSYS) | Some(libc::EPERM))
    }

    /// A `sh` in a session of its own with one background sleeper in it — the
    /// shape a PTY child has, and the only shape where a session sweep does
    /// something a `kill(-pgid, …)` would not.
    #[cfg(target_os = "linux")]
    struct SessionLeader {
        child: Option<Child>,
        leader: i32,
    }

    #[cfg(target_os = "linux")]
    impl SessionLeader {
        fn spawn() -> Self {
            use std::os::unix::process::CommandExt;

            let mut command = Command::new("sh");
            // Keep the leader alive independently of the background member.
            // With `sleep 30 & wait`, killing the member can wake `wait` and
            // let the shell exit before the direct SIGKILL reaches it, making
            // the test observe a normal/137 exit even though the session was
            // drained correctly. `exec` leaves one stable leader and one
            // background member, so both halves of the invariant are
            // deterministic.
            command.args(["-c", "sleep 30 & exec sleep 30"]);
            // SAFETY: setsid is async-signal-safe and the closure performs no
            // allocation or other work after Command forks.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
            let child = command.spawn().expect("spawn session leader");
            let leader = child.id() as i32;
            Self {
                child: Some(child),
                leader,
            }
        }

        fn live_members(&self) -> Vec<i32> {
            std::fs::read_dir("/proc")
                .expect("read proc")
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
                .filter(|pid| {
                    *pid != self.leader
                        && process_stat(*pid)
                            .is_some_and(|stat| stat.session == self.leader && stat.is_live())
                })
                .collect()
        }

        /// Block until the background sleeper has joined the session.
        fn await_member(&self) -> i32 {
            let mut member = None;
            let appeared = wait_for(Duration::from_secs(2), || {
                member = self.live_members().first().copied();
                member.is_some()
            });
            assert!(appeared, "background session member did not appear");
            member.expect("a member was observed")
        }

        fn wait(&mut self) -> std::process::ExitStatus {
            self.child
                .take()
                .expect("the leader has not been reaped yet")
                .wait()
                .expect("reap session leader")
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for SessionLeader {
        fn drop(&mut self) {
            // Only while the pid is still held: once reaped it may be reused.
            if let Some(mut child) = self.child.take() {
                unsafe {
                    libc::kill(-self.leader, libc::SIGKILL);
                    libc::kill(self.leader, libc::SIGKILL);
                }
                let _ = child.wait();
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[allow(clippy::zombie_processes)] // ChildLifecycle exclusively owns waitpid.
    fn natural_leader_exit_cleans_same_group_descendants_before_reaping() {
        use std::os::unix::process::CommandExt;

        let root = std::env::temp_dir().join(format!(
            "jterm-natural-exit-descendants-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let marker = root.join("descendant-survived");
        let mut command = Command::new("/bin/sh");
        // The background job shares the non-interactive shell's group and
        // ignores its hangup. Bound the fixture so a failing test cannot leave
        // a long-running orphan. A separate session is not owned by this test.
        command
            .args([
                "-c",
                "(trap '' HUP TERM; sleep 0.6; printf survived > \"$1\") & exit 23",
                "natural-exit-test",
            ])
            .arg(&marker);
        // SAFETY: setsid is async-signal-safe in the freshly forked child.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let child = command.spawn().expect("spawn natural-exit fixture");
        let pid = child.id() as i32;
        let lifecycle = ChildLifecycle::new(pid, ReapOwner::Ours).unwrap();
        assert!(wait_for(Duration::from_secs(2), || lifecycle
            .poll_reap()
            .is_some()));
        assert_eq!(
            lifecycle.exit_code(),
            Some(23),
            "preserve the real leader exit"
        );
        assert!(!lifecycle.terminate(EscalationPolicy::SESSION_DRAIN));
        assert!(is_fully_reaped(pid));
        drop(lifecycle);
        std::thread::sleep(Duration::from_millis(850));
        let survived = marker.exists();
        let _ = std::fs::remove_dir_all(root);
        assert!(
            !survived,
            "natural leader exit left a same-group descendant running"
        );
    }

    // -- hoisted from anvil ------------------------------------------------

    #[test]
    fn session_drain_requires_quiet_after_a_late_fork_appears() {
        let mut scans = std::collections::VecDeque::from([1usize, 0, 1, 0, 0]);
        let mut calls = 0usize;
        let quiet = repeat_session_scans_until_quiet(
            Instant::now() + Duration::from_secs(1),
            Duration::ZERO,
            REQUIRED_QUIET_SESSION_SCANS,
            || {
                calls += 1;
                scans.pop_front().unwrap_or_default()
            },
        );
        assert!(quiet);
        assert_eq!(calls, 5);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_signal_is_bound_to_the_opened_process() {
        let mut child = spawn_sleeper();
        let pidfd = match open_pidfd(child.id() as i32) {
            Ok(pidfd) => pidfd,
            Err(error) if pidfd_is_unavailable(&error) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("pidfd_open failed: {error}");
            }
        };
        if let Err(error) = send_pidfd_signal(&pidfd, libc::SIGKILL) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("pidfd_send_signal failed: {error}");
        }
        let status = child.wait().expect("reap pidfd target");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn final_drain_kills_a_background_member_of_the_child_session() {
        let mut session = SessionLeader::spawn();
        let member = session.await_member();
        if let Err(error) = open_pidfd(member) {
            assert!(
                pidfd_is_unavailable(&error),
                "pidfd_open for session member failed: {error}"
            );
            return;
        }

        let leader_ref = ProcessRef::open(session.leader).expect("reference the session leader");
        signal_process_tree_until_quiet(
            &leader_ref,
            libc::SIGKILL,
            &EscalationPolicy::SESSION_DRAIN,
        );
        let survivors = session.live_members();
        let status = session.wait();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(
            survivors.is_empty(),
            "live descendants {survivors:?} remained in the drained PTY session"
        );
    }

    #[test]
    #[allow(clippy::zombie_processes)] // ChildLifecycle, not Child, owns waitpid here.
    fn child_lifecycle_caches_reaped_status_for_later_drop_paths() {
        let child = Command::new("sh")
            .args(["-c", "exit 23"])
            .spawn()
            .expect("spawn exiting child");
        let lifecycle =
            ChildLifecycle::new(child.id() as i32, ReapOwner::Ours).expect("reference the child");
        assert!(
            wait_for(Duration::from_secs(2), || lifecycle.poll_reap().is_some()),
            "child was not reaped before the test deadline"
        );
        assert_eq!(lifecycle.poll_reap(), Some(23));
        // The cached status is what makes every later drop path safe.
        assert_eq!(lifecycle.exit_code(), Some(23));
        assert_eq!(lifecycle.poll_reap(), Some(23));
    }

    #[test]
    fn an_externally_reaped_child_fails_closed_without_later_signals() {
        let mut child = Command::new("sh")
            .args(["-c", "exit 23"])
            .spawn()
            .expect("spawn externally reaped child");
        let lifecycle =
            ChildLifecycle::new(child.id() as i32, ReapOwner::Ours).expect("reference child");
        assert_eq!(child.wait().unwrap().code(), Some(23));
        assert_eq!(lifecycle.poll_reap(), Some(1), "ECHILD has unknown status");
        assert_eq!(lifecycle.exit_code(), Some(1));
        assert!(!lifecycle.terminate(EscalationPolicy::SESSION_DRAIN));
        assert_eq!(
            lifecycle.signal(libc::SIGKILL).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        lifecycle.force_kill_and_reap();
        assert_eq!(lifecycle.exit_code(), Some(1));
    }

    // -- new surface --------------------------------------------------------

    #[test]
    fn the_presets_share_one_ladder_and_differ_only_in_the_session_sweep() {
        let drain = EscalationPolicy::SESSION_DRAIN;
        let group = EscalationPolicy::PROCESS_GROUP;
        assert!(drain.drain_session);
        assert!(!group.drain_session);
        assert!(EscalationPolicy::default().drain_session);
        // anvil's timings, which both presets inherit.
        assert_eq!(drain.hangup_signal, libc::SIGHUP);
        assert_eq!(drain.term_signal, libc::SIGTERM);
        assert_eq!(drain.kill_signal, libc::SIGKILL);
        assert_eq!(drain.drain_timeout, SESSION_KILL_DRAIN_TIMEOUT);
        assert_eq!(drain.scan_interval, SESSION_SCAN_INTERVAL);
        assert_eq!(drain.required_quiet_scans, REQUIRED_QUIET_SESSION_SCANS);
        assert_eq!(group.hangup_signal, drain.hangup_signal);
        assert_eq!(group.term_delay, drain.term_delay);
        assert_eq!(group.kill_delay, drain.kill_delay);
        assert_eq!(group.drain_timeout, drain.drain_timeout);
    }

    /// The defining property of the `PROCESS_GROUP` preset: a caller that owns
    /// only a process group must not have its shutdown enumerate `/proc` and
    /// signal processes it never spawned.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_process_group_preset_never_sweeps_the_session() {
        let session = SessionLeader::spawn();
        session.await_member();
        let leader_ref = ProcessRef::open(session.leader).expect("reference the session leader");

        // Signal 0 probes for existence without disturbing anything.
        let group_only = signal_process_tree(&leader_ref, 0, false);
        assert!(
            !group_only.complete && group_only.live_members == 0,
            "PROCESS_GROUP must not enumerate /proc: {group_only:?}"
        );

        let swept = signal_process_tree(&leader_ref, 0, true);
        assert!(swept.complete, "SESSION_DRAIN must enumerate /proc");
        assert!(
            swept.live_members >= 1,
            "the session sweep must see the background member"
        );

        // Nothing was killed by a null signal; the fixture's Drop cleans up.
        assert!(!session.live_members().is_empty());
    }

    #[test]
    fn terminate_is_idempotent_and_starts_exactly_one_escalation() {
        let child = spawn_sleeper();
        let pid = child.id() as i32;
        std::mem::forget(child); // The lifecycle owns waitpid from here on.
        let lifecycle = ChildLifecycle::new(pid, ReapOwner::Ours).expect("reference the child");

        let policy = EscalationPolicy {
            term_delay: Duration::from_millis(5),
            kill_delay: Duration::from_millis(5),
            drain_timeout: Duration::from_millis(50),
            reap_poll_interval: Duration::from_millis(5),
            ..EscalationPolicy::PROCESS_GROUP
        };
        assert!(
            lifecycle.terminate(policy),
            "first terminate runs the ladder"
        );
        assert!(
            !lifecycle.terminate(policy),
            "a second terminate must not start a second escalation thread"
        );

        assert!(
            wait_for(Duration::from_secs(5), || lifecycle.exit_code().is_some()),
            "the escalation worker did not reap the child"
        );
        let code = lifecycle.exit_code().expect("the worker cached a status");
        assert!(
            matches!(
                signal_name_for_exit(code),
                Some("SIGHUP") | Some("SIGTERM") | Some("SIGKILL")
            ),
            "expected a ladder signal code, got {code}"
        );
        // Terminating an already-reaped child is a no-op too.
        assert!(!lifecycle.terminate(policy));
        assert!(lifecycle.signal(libc::SIGKILL).is_err());
    }

    #[test]
    fn foreign_owner_never_reaps_the_childs_status() {
        let mut child = Command::new("sh")
            .args(["-c", "exit 23"])
            .spawn()
            .expect("spawn exiting child");
        let lifecycle = ChildLifecycle::new(child.id() as i32, ReapOwner::Foreign)
            .expect("reference the child");

        // Poll well past the child's exit; a Foreign lifecycle must never call
        // waitpid, so it can never learn the status this way.
        let stole = wait_for(Duration::from_millis(300), || {
            lifecycle.poll_reap().is_some()
        });
        assert!(!stole, "a Foreign lifecycle called waitpid");

        // The real owner still finds the status waiting for it.
        let status = child.wait().expect("the foreign owner reaps its own child");
        assert_eq!(status.code(), Some(23));

        assert_eq!(lifecycle.poll_reap(), None);
        lifecycle.note_foreign_exit(23);
        assert_eq!(lifecycle.poll_reap(), Some(23));
        assert_eq!(lifecycle.exit_code(), Some(23));
        assert_eq!(
            lifecycle.exit_outcome(),
            Some(ChildExitOutcome::ForeignCode(23))
        );
        // A retired pid may already belong to somebody else.
        assert!(lifecycle.signal(libc::SIGTERM).is_err());
        // Dropping must not waitpid a pid the foreign owner already released.
        drop(lifecycle);
    }

    #[test]
    fn foreign_force_kill_leaves_the_status_for_its_owner() {
        let mut child = spawn_sleeper();
        let lifecycle = ChildLifecycle::new(child.id() as i32, ReapOwner::Foreign)
            .expect("reference the child");
        lifecycle.force_kill_and_reap();
        assert_eq!(
            lifecycle.exit_code(),
            None,
            "a Foreign force-kill must not consume the status"
        );
        let status = child.wait().expect("the foreign owner reaps its own child");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        drop(lifecycle);
    }

    #[test]
    fn drop_reaps_a_child_that_was_never_terminated() {
        let child = spawn_sleeper();
        let pid = child.id() as i32;
        std::mem::forget(child); // The lifecycle owns waitpid from here on.
        let lifecycle = ChildLifecycle::new(pid, ReapOwner::Ours).expect("reference the child");
        assert_eq!(lifecycle.poll_reap(), None);
        drop(lifecycle);
        assert!(
            is_fully_reaped(pid),
            "Drop left a zombie behind for pid {pid}"
        );
    }

    #[test]
    fn process_ref_rejects_pids_that_cannot_be_signaled_safely() {
        assert_eq!(
            ProcessRef::open(0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            ProcessRef::open(-1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(ChildLifecycle::new(0, ReapOwner::Ours).is_err());
        assert!(ChildLifecycle::new(-1, ReapOwner::Foreign).is_err());
    }

    #[test]
    fn process_ref_falls_back_to_a_raw_pid_when_pidfd_is_unavailable() {
        let mut child = spawn_sleeper();
        let pid = child.id() as i32;

        let opened = ProcessRef::open(pid).expect("reference a live child");
        assert_eq!(opened.pid(), pid);
        #[cfg(target_os = "linux")]
        {
            // Kernel-bound unless pidfd_open is unavailable, in which case
            // `open` already degraded to the fallback under test below.
            assert_eq!(opened.is_kernel_bound(), open_pidfd(pid).is_ok());
        }
        drop(opened);

        // The path taken on non-Linux and on ENOSYS/EPERM kernels: no pidfd,
        // signals go straight to the pid, and callers can tell.
        let fallback = ProcessRef::unverified(pid);
        assert!(!fallback.is_kernel_bound());
        assert_eq!(fallback.pid(), pid);
        fallback
            .signal(libc::SIGKILL)
            .expect("the raw-pid fallback delivers signals");
        let status = child.wait().expect("reap the signaled child");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }

    #[test]
    fn process_group_signals_require_verified_group_leadership() {
        let mut child = spawn_sleeper();
        let pid = child.id() as i32;
        let process = ProcessRef::open(pid).expect("reference a live child");

        // A plain Command child inherits this process' group, so it leads no
        // group of its own and the negative-pid kill must be refused.
        if process_stat(pid).is_some_and(|stat| stat.process_group != pid) {
            assert_eq!(
                process
                    .signal_process_group(libc::SIGKILL)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        process.signal(libc::SIGKILL).expect("signal the child");
        let status = child.wait().expect("reap the signaled child");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }

    #[test]
    #[allow(clippy::zombie_processes)] // ChildLifecycle, not Child, owns waitpid here.
    fn exit_codes_follow_the_familys_signal_offset_convention() {
        let exited = Command::new("sh")
            .args(["-c", "exit 23"])
            .spawn()
            .expect("spawn exiting child");
        let lifecycle =
            ChildLifecycle::new(exited.id() as i32, ReapOwner::Ours).expect("reference the child");
        assert!(wait_for(Duration::from_secs(2), || lifecycle
            .poll_reap()
            .is_some()));
        assert_eq!(lifecycle.poll_reap(), Some(23));
        assert_eq!(lifecycle.exit_outcome(), Some(ChildExitOutcome::Exited(23)));
        assert_eq!(signal_name_for_exit(23), None, "23 is a plain exit code");

        let signaled = spawn_sleeper();
        let pid = signaled.id() as i32;
        std::mem::forget(signaled); // The lifecycle owns waitpid from here on.
        let lifecycle = ChildLifecycle::new(pid, ReapOwner::Ours).expect("reference the child");
        lifecycle.signal(libc::SIGKILL).expect("signal the child");
        assert!(wait_for(Duration::from_secs(2), || lifecycle
            .poll_reap()
            .is_some()));
        let code = lifecycle.poll_reap().expect("a cached status");
        assert_eq!(code, 128 + libc::SIGKILL);
        assert_eq!(
            lifecycle.poll_reap_outcome(),
            Some(ChildExitOutcome::Signaled(
                NonZeroU8::new(libc::SIGKILL as u8).unwrap()
            ))
        );
        assert_eq!(
            signal_name_for_exit(code),
            Some("SIGKILL"),
            "the 128+signal convention must round-trip through exit_status"
        );
    }

    #[test]
    fn nonconsuming_observation_distinguishes_exits_from_signals() {
        let shell_code = 128 + libc::SIGKILL;
        let mut exited = Command::new("sh")
            .args(["-c", &format!("exit {shell_code}")])
            .spawn()
            .expect("spawn an exiting child");
        let exit_pid = exited.id() as i32;
        let exit = observe_child_exit(exit_pid, false);
        let exit_status = exited.wait().expect("observe leaves the exit waitable");
        let expected_exit = ChildExitOutcome::Exited(shell_code as u8);
        assert_eq!(exit.unwrap(), Some(expected_exit));
        assert_eq!(
            decode_wait_status(exit_status.into_raw()),
            Some(expected_exit)
        );
        assert_eq!(expected_exit.negative_signal_code(), Some(shell_code));

        let mut signaled = spawn_sleeper();
        let signal_pid = signaled.id() as i32;
        let running = observe_child_exit(signal_pid, true);
        signaled.kill().expect("signal the test child");
        let signal = observe_child_exit(signal_pid, false);
        let signal_status = signaled.wait().expect("observe leaves the signal waitable");
        let expected_signal =
            ChildExitOutcome::Signaled(NonZeroU8::new(libc::SIGKILL as u8).unwrap());
        assert_eq!(running.unwrap(), None);
        assert_eq!(signal.unwrap(), Some(expected_signal));
        assert_eq!(
            decode_wait_status(signal_status.into_raw()),
            Some(expected_signal)
        );
        assert_eq!(expected_signal.shell_code(), shell_code);
        assert_eq!(expected_signal.negative_signal_code(), Some(-libc::SIGKILL));

        assert_eq!(
            observe_child_exit(signal_pid, true)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ECHILD)
        );
        assert_eq!(
            observe_child_exit(0, true).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(decode_wait_status(-1), None);
    }

    #[test]
    fn lost_wait_status_stays_distinct_from_a_real_failure_exit() {
        let mut child = Command::new("sh")
            .args(["-c", "exit 1"])
            .spawn()
            .expect("spawn exiting child");
        let lifecycle = ChildLifecycle::new(child.id() as i32, ReapOwner::Ours)
            .expect("reference the child before another owner reaps it");
        assert_eq!(child.wait().expect("reap child externally").code(), Some(1));
        assert_eq!(
            lifecycle.poll_reap_outcome(),
            Some(ChildExitOutcome::Unavailable)
        );
        assert_eq!(
            lifecycle.exit_outcome(),
            Some(ChildExitOutcome::Unavailable)
        );
        assert_eq!(lifecycle.exit_code(), Some(1));
        assert_eq!(ChildExitOutcome::Unavailable.negative_signal_code(), None);
        assert!(lifecycle.signal(libc::SIGTERM).is_err());
    }

    #[test]
    fn stops_and_continues_are_not_terminal_statuses() {
        let mut child = spawn_sleeper();
        let pid = child.id() as i32;
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);

        let mut status: c_int = 0;
        let waited = loop {
            let waited = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
            if waited < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break waited;
        };
        assert_eq!(waited, pid);
        assert!(libc::WIFSTOPPED(status));
        assert_eq!(decode_wait_status(status), None);
        assert_eq!(
            exit_code_from_wait_status(status),
            None,
            "a stop report must never be mistaken for an exit"
        );

        // SIGKILL reaches a stopped process without SIGCONT.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        let status = child.wait().expect("reap the killed child");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }
}
