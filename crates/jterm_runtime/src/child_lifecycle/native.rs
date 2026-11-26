//! Exclusive negative-signal status and wait ownership for native PTY adapters.
use super::{decode_wait_status, observe_child_exit, ChildExitOutcome};
use std::{io, time::Duration};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeChildPhase {
    Running,
    TerminationStarted,
    Reaped,
}

/// The native transport owns this state under its session lock. Once a
/// detached reaper takes the child, every later actor/drop path leaves that
/// exact wait owner alone. Session observation stays with the host callback,
/// which must never consume or transfer this direct child's wait status.
#[derive(Debug)]
pub struct NativePtyChild {
    pid: i32,
    phase: NativeChildPhase,
    code: Option<i32>,
    detached: bool,
}

fn retry<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match operation() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn wait_owned(pid: i32) -> io::Result<ChildExitOutcome> {
    let mut status = 0;
    let waited = retry(|| {
        // SAFETY: only the exclusive owner calls this for its unreaped child.
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result)
        }
    });
    match waited {
        Ok(child) if child == pid => {
            Ok(decode_wait_status(status).unwrap_or(ChildExitOutcome::Unavailable))
        }
        Ok(_) => Err(io::Error::other(
            "waitpid returned no status for an observed PTY exit",
        )),
        Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
            Ok(ChildExitOutcome::Unavailable)
        }
        Err(error) => Err(error),
    }
}

fn kill_owned(pid: i32) -> io::Result<ChildExitOutcome> {
    // Even an exited leader must remain unreaped until the final group
    // signal. ECHILD never authorizes using a potentially recycled number.
    match retry(|| observe_child_exit(pid, true)) {
        Ok(_) => {}
        Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
            return Ok(ChildExitOutcome::Unavailable)
        }
        Err(error) => return Err(error),
    }
    // SAFETY: observation retained this direct child's PID/PGID ownership.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
        libc::kill(pid, libc::SIGKILL);
    }
    wait_owned(pid)
}

impl NativePtyChild {
    /// Adopt the positive direct-child PID supplied by the shared launcher.
    /// The caller transfers exclusive wait ownership and retains its drop
    /// adapter until termination has either completed or been delegated.
    pub fn new(pid: i32) -> io::Result<Self> {
        if pid <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid child PID",
            ));
        }
        Ok(Self {
            pid,
            phase: NativeChildPhase::Running,
            code: None,
            detached: false,
        })
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }
    pub fn phase(&self) -> NativeChildPhase {
        self.phase
    }
    pub fn cached_code(&self) -> Option<i32> {
        self.code
    }

    /// Retire only after ECHILD proves that this owner lost the wait status.
    pub fn retire_unknown(&mut self) {
        self.cache(-1);
    }

    fn cache(&mut self, code: i32) {
        self.code = Some(code);
        self.phase = NativeChildPhase::Reaped;
    }

    /// One syscall, so deadline-bound native callers can budget EINTR retries.
    pub fn observe_once(pid: i32, nonblocking: bool) -> io::Result<bool> {
        observe_child_exit(pid, nonblocking).map(|status| status.is_some())
    }

    pub fn observe(pid: i32, nonblocking: bool) -> io::Result<bool> {
        retry(|| Self::observe_once(pid, nonblocking))
    }

    /// Drain other process groups while the waitable leader still reserves
    /// its numeric session identity, then kill its private group and reap it.
    /// A failed/incomplete drain keeps the exact owner and cached status open.
    /// The callback may settle other session groups, but must not wait/reap
    /// this leader or transfer its wait ownership to another thread.
    pub fn finish_observed_exit(
        &mut self,
        drain: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<i32> {
        if let Some(code) = self.code {
            return Ok(code);
        }
        match Self::observe(self.pid, true) {
            Ok(true) => {}
            Ok(false) => return Err(io::ErrorKind::WouldBlock.into()),
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                self.retire_unknown();
                return Ok(-1);
            }
            Err(error) => return Err(error),
        }
        drain()?;
        // Fail closed if a foreign callback/reaper violated the wait contract.
        match Self::observe(self.pid, true) {
            Ok(true) => {}
            Ok(false) => return Err(io::ErrorKind::WouldBlock.into()),
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                self.retire_unknown();
                return Ok(-1);
            }
            Err(error) => return Err(error),
        }
        // SAFETY: WNOWAIT has kept the owned leader unreaped through drain.
        if unsafe { libc::kill(-self.pid, libc::SIGKILL) } < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        let code = wait_owned(self.pid)?.negative_signal_code().unwrap_or(-1);
        self.cache(code);
        Ok(code)
    }

    pub fn try_reap(&mut self, drain: impl FnOnce() -> io::Result<()>) -> Option<i32> {
        if let Some(code) = self.code {
            return Some(code);
        }
        match Self::observe(self.pid, true) {
            Ok(false) => None,
            Ok(true) => match self.finish_observed_exit(drain) {
                Ok(code) => Some(code),
                Err(error) => {
                    log::warn!("could not finish observed PTY child exit: {error}");
                    None
                }
            },
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                self.retire_unknown();
                Some(-1)
            }
            Err(error) => {
                log::warn!("waitid(WNOWAIT) failed while observing PTY child: {error}");
                None
            }
        }
    }

    pub fn is_alive(&mut self, drain: impl FnOnce() -> io::Result<()>) -> bool {
        if self.phase != NativeChildPhase::Running || self.code.is_some() {
            return false;
        }
        self.try_reap(drain).is_none()
    }

    fn graceful_signals(&self) {
        // SAFETY: the direct child is still owned and has a private group.
        unsafe {
            libc::kill(-self.pid, libc::SIGHUP);
            libc::kill(self.pid, libc::SIGTERM);
        }
    }

    /// Actor-owned graceful stop; destructive wait remains with this object.
    pub fn signal_terminate(&mut self) {
        if self.code.is_some() || self.phase != NativeChildPhase::Running {
            return;
        }
        match Self::observe(self.pid, true) {
            Ok(_) => self.graceful_signals(),
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => self.retire_unknown(),
            Err(error) => log::warn!("could not observe terminating PTY child: {error}"),
        }
    }

    pub fn begin_termination(&mut self, drain: impl FnOnce() -> io::Result<()>) -> bool {
        if !self.is_alive(drain) {
            return false;
        }
        // is_alive deliberately keeps unknown observation failures live.
        // Such a failure does not authorize using a numeric signal target.
        match Self::observe(self.pid, true) {
            Ok(_) => {}
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                self.retire_unknown();
                return false;
            }
            Err(error) => {
                log::warn!("could not verify terminating PTY child: {error}");
                return false;
            }
        }
        self.phase = NativeChildPhase::TerminationStarted;
        self.code = Some(-libc::SIGTERM);
        self.graceful_signals();
        true
    }

    /// The delayed worker becomes the sole destructive wait owner before
    /// this returns. Later actor force-kill callbacks cannot race its wait.
    pub fn terminate(&mut self, grace: Duration, drain: impl FnOnce() -> io::Result<()>) {
        if !self.begin_termination(drain) {
            return;
        }
        let pid = self.pid;
        match std::thread::Builder::new()
            .name("jterm-native-pty-cleanup".into())
            .spawn(move || {
                std::thread::sleep(grace);
                if let Err(error) = kill_owned(pid) {
                    log::warn!("could not reap terminating PTY child: {error}");
                }
            }) {
            Ok(_) => self.detached = true,
            Err(error) => {
                log::error!("could not start PTY cleanup worker: {error}");
                self.finish_termination();
            }
        }
    }

    /// Ember's immediate actor cleanup caches its real wait status; its
    /// already-delegated termination code remains the original -SIGTERM.
    pub fn force_kill_and_reap(&mut self) {
        if self.code.is_some() || self.detached {
            return;
        }
        match kill_owned(self.pid) {
            Ok(outcome) => self.cache(outcome.negative_signal_code().unwrap_or(-libc::SIGKILL)),
            Err(error) => log::warn!("could not reap killed PTY child: {error}"),
        }
    }

    /// Frost's actor-owned escalation retains the already-published -SIGTERM
    /// result. A separately detached reaper owns its child until it finishes.
    pub fn finish_termination(&mut self) {
        if self.phase != NativeChildPhase::TerminationStarted || self.detached {
            return;
        }
        match kill_owned(self.pid) {
            Ok(_) => self.phase = NativeChildPhase::Reaped,
            Err(error) => log::warn!("could not reap terminating PTY child: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    fn child(script: &str) -> NativePtyChild {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: setsid is async-signal-safe and the forked child owns its session.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        #[expect(
            clippy::zombie_processes,
            reason = "the fixture transfers the wait to NativePtyChild"
        )]
        let spawned = command.spawn().unwrap();
        NativePtyChild::new(spawned.id() as i32).unwrap()
    }

    fn exited(child: &NativePtyChild) {
        let started = Instant::now();
        while !NativePtyChild::observe(child.pid(), true).unwrap() {
            assert!(started.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn drain_failure_retains_original_status_and_unreaped_identity() {
        let mut owner = child("exit 23");
        exited(&owner);
        let error = owner
            .finish_observed_exit(|| Err(io::ErrorKind::WouldBlock.into()))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(owner.phase(), NativeChildPhase::Running);
        assert_eq!(owner.cached_code(), None);
        assert!(NativePtyChild::observe(owner.pid(), true).unwrap());
        assert_eq!(owner.finish_observed_exit(|| Ok(())).unwrap(), 23);
        assert_eq!(owner.cached_code(), Some(23));
        assert_eq!(owner.phase(), NativeChildPhase::Reaped);
        assert_eq!(
            NativePtyChild::observe(owner.pid(), true)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ECHILD)
        );
        assert_eq!(
            owner
                .finish_observed_exit(|| panic!("cached exit must not drain again"))
                .unwrap(),
            23
        );
    }

    #[test]
    fn callback_cannot_release_an_anchor_and_authorize_a_later_group_signal() {
        let mut owner = child("exit 23");
        exited(&owner);
        let pid = owner.pid();
        let result = owner
            .finish_observed_exit(|| {
                assert!(matches!(wait_owned(pid)?, ChildExitOutcome::Exited(23)));
                Ok(())
            })
            .unwrap();
        assert_eq!(result, -1);
        assert_eq!(owner.cached_code(), Some(-1));
        assert_eq!(owner.phase(), NativeChildPhase::Reaped);
    }

    #[test]
    fn syscall_retry_preserves_non_interruption_errors() {
        let mut attempts = 0;
        assert_eq!(
            retry(|| {
                attempts += 1;
                if attempts == 1 {
                    Err(io::ErrorKind::Interrupted.into())
                } else {
                    Ok(7)
                }
            })
            .unwrap(),
            7
        );
        assert_eq!(attempts, 2);
        let mut attempts = 0;
        let error = retry::<()>(|| {
            attempts += 1;
            Err(io::ErrorKind::PermissionDenied.into())
        })
        .unwrap_err();
        assert_eq!(attempts, 1);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn immediate_force_preserves_real_negative_sighup_instead_of_unknown_fallback() {
        let mut owner = child("kill -HUP $$");
        exited(&owner);
        owner.force_kill_and_reap();
        assert_eq!(owner.cached_code(), Some(-libc::SIGHUP));
        assert_eq!(owner.phase(), NativeChildPhase::Reaped);
    }

    #[test]
    fn lost_wait_status_retires_identity_without_clean_success() {
        let mut owner = child("exit 23");
        exited(&owner);
        assert!(matches!(
            wait_owned(owner.pid()).unwrap(),
            ChildExitOutcome::Exited(23)
        ));
        assert_eq!(
            owner.try_reap(|| panic!("unowned child must not authorize drain")),
            Some(-1)
        );
        assert_eq!(owner.phase(), NativeChildPhase::Reaped);
        owner.signal_terminate();
        owner.force_kill_and_reap();
        assert_eq!(owner.cached_code(), Some(-1));
    }

    #[test]
    fn detached_reaper_remains_the_exclusive_wait_owner_after_actor_force() {
        let mut owner = child("trap '' HUP TERM; while :; do sleep 0.01; done");
        let pid = owner.pid();
        owner.terminate(Duration::from_millis(150), || Ok(()));
        assert_eq!(owner.phase(), NativeChildPhase::TerminationStarted);
        owner.finish_termination();
        owner.force_kill_and_reap();
        assert_eq!(owner.phase(), NativeChildPhase::TerminationStarted);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match NativePtyChild::observe(pid, true) {
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => break,
                _ => assert!(Instant::now() < deadline, "detached child was not reaped"),
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(owner.cached_code(), Some(-libc::SIGTERM));
    }

    #[test]
    fn invalid_anchor_never_authorizes_signaling_the_calling_process() {
        const FLAG: &str = "JTERM_TEST_NATIVE_CHILD_INVALID_ANCHOR";
        if std::env::var_os(FLAG).is_none() {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "child_lifecycle::native::tests::invalid_anchor_never_authorizes_signaling_the_calling_process", "--test-threads=1"]).env(FLAG, "1");
            // SAFETY: isolate this deliberate invalid-anchor regression from the parent group.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
            assert!(command.status().unwrap().success());
            return;
        }
        for pid in [0, -1, -2] {
            assert!(NativePtyChild::new(pid).is_err());
        }
        let mut owner = NativePtyChild::new(std::process::id() as i32).unwrap();
        owner.signal_terminate();
        assert_eq!(owner.cached_code(), Some(-1));
        owner.force_kill_and_reap();
        assert_eq!(owner.phase(), NativeChildPhase::Reaped);
    }
}
