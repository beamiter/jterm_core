//! Reuse-safe ownership for short-lived, application-controlled helpers.
//!
//! On Unix, a child in this module is the leader of a fresh process group. The
//! root is observed with `waitid(..., WNOWAIT)`, which deliberately leaves it
//! as a zombie until the group has been signalled. Keeping that PID allocated
//! closes the otherwise tiny window where `kill(-pgid, ...)` could address an
//! unrelated group after an ordinary destructive status poll reaped the leader.
//! Platforms without that process-group contract cache the status consumed by
//! `try_wait` and return it without attempting a second kill or wait.
//!
//! Unix callers must remain the sole waiter for children owned by this module
//! and must not install an auto-reaping SIGCHLD disposition while a supervised
//! child exists. Spawn rejects SIG_IGN and SA_NOCLDWAIT; a custom handler is
//! allowed only when it never calls waitpid for these children (including a
//! catch-all waitpid(-1)). The disposition and wait ownership are checked again
//! immediately before signalling, but another thread racing a foreign waitpid
//! or sigaction between that check and kill is outside this primitive's
//! enforceable contract.

use std::io;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};

/// An owned helper child whose process group is cleared before reap on Unix.
///
/// The underlying [`Child`] is deliberately opaque: callers may take its
/// standard streams, observe the root without reaping it, and finish the
/// owned cleanup, but cannot accidentally poll its status destructively and
/// release the PID early.
pub struct SupervisedChild {
    child: Option<Child>,
    #[cfg(unix)]
    process_group: i32,
    #[cfg(any(not(unix), test))]
    cached_status: Option<ExitStatus>,
}

#[cfg(unix)]
fn validate_unix_child(mut child: Child) -> io::Result<(Child, i32)> {
    let process_group = match i32::try_from(child.id()) {
        Ok(pid) if pid > 1 => pid,
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "helper child did not have a safe process-group id",
            ));
        }
    };

    // SAFETY: getpgrp has no preconditions and only reads process state.
    if process_group == unsafe { libc::getpgrp() } {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "helper child joined the caller's process group",
        ));
    }
    Ok((child, process_group))
}

#[cfg(unix)]
fn require_waitable_sigchld() -> io::Result<()> {
    // SAFETY: action is writable and a null new-action pointer makes sigaction
    // a read-only query of the process-wide disposition.
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    let result = unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if action.sa_sigaction == libc::SIG_IGN || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "supervised helpers require a waitable SIGCHLD disposition",
        ));
    }
    Ok(())
}

impl SupervisedChild {
    /// Spawn `command`; on Unix, make it leader of a fresh process group.
    ///
    /// Fails with [`io::ErrorKind::Unsupported`] without spawning when the
    /// process-wide SIGCHLD disposition would auto-reap the child (`SIG_IGN`
    /// or `SA_NOCLDWAIT`): the supervision contract needs a waitable child.
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            require_waitable_sigchld()?;
        }

        #[cfg(unix)]
        let (child, process_group) = validate_unix_child(command.spawn()?)?;
        #[cfg(not(unix))]
        let child = command.spawn()?;

        Ok(Self {
            child: Some(child),
            #[cfg(unix)]
            process_group,
            #[cfg(any(not(unix), test))]
            cached_status: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn id(&self) -> u32 {
        self.child
            .as_ref()
            .expect("supervised child used after reap")
            .id()
    }

    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.as_mut()?.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.as_mut()?.stderr.take()
    }

    /// Observe a terminal root status without consuming it.
    ///
    /// A `true` result means the root remains a waitable zombie.  The caller
    /// must promptly call [`Self::reap_after_group_kill`], which signals the
    /// still-unrecycled group before consuming that status.
    #[cfg(unix)]
    pub fn root_has_exited(&mut self) -> io::Result<bool> {
        if self.child.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "supervised child was already reaped",
            ));
        }
        self.probe_unix_root()
    }

    #[cfg(unix)]
    fn probe_unix_root(&self) -> io::Result<bool> {
        let pid = self.process_group;
        loop {
            // Linux documents a zero si_pid for WNOHANG with no waitable
            // state. Start from zero as required by older POSIX revisions too.
            // SAFETY: all-zero is a valid initial representation for the
            // output siginfo_t passed to waitid.
            let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
            // SAFETY: info points to writable storage, pid names the exact
            // direct child we own, and WNOWAIT deliberately retains status.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                // SAFETY: waitid initialized `info`; si_pid is the documented
                // discriminator for a WNOHANG call that found no status.
                return Ok(unsafe { info.si_pid() } == pid);
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
    }

    #[cfg(not(unix))]
    pub fn root_has_exited(&mut self) -> io::Result<bool> {
        if self.cached_status.is_some() {
            return Ok(true);
        }
        let child = self.child.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "supervised child was already reaped",
            )
        })?;
        match child.try_wait()? {
            Some(status) => {
                self.cached_status = Some(status);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Signal the owned process group, directly kill the root as a fallback,
    /// and synchronously consume its status.
    ///
    /// When called after [`Self::root_has_exited`] returned true, the SIGKILL
    /// cannot change the root's already-recorded status.  It only clears any
    /// descendants that inherited the group.
    pub fn reap_after_group_kill(&mut self) -> io::Result<ExitStatus> {
        self.reap_after_group_kill_with(|_| {})
    }

    fn reap_after_group_kill_with(
        &mut self,
        before_group_signal: impl FnOnce(i32),
    ) -> io::Result<ExitStatus> {
        #[cfg(any(not(unix), test))]
        if let Some(status) = self.cached_status.take() {
            // A portable try_wait already consumed the status and released
            // the PID. Disarm before returning and never invoke the signal
            // hook with a numeric identifier that may have been recycled.
            self.child = None;
            return Ok(status);
        }
        if self.child.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "supervised child was already reaped",
            ));
        }
        #[cfg(unix)]
        {
            // Prove that this process can still wait for the exact root before
            // using its numeric PID as a process-group address. ECHILD (or any
            // other ownership/probe failure) permanently disarms without a
            // signal, because that identifier may already be recyclable.
            if let Err(error) = self.probe_unix_root() {
                self.child = None;
                return Err(error);
            }
            if let Err(error) = require_waitable_sigchld() {
                self.child = None;
                return Err(error);
            }
            before_group_signal(self.process_group);
            self.signal_group();
        }
        #[cfg(not(unix))]
        drop(before_group_signal);

        // On Unix the group has now been signalled; portable targets have no
        // raw group signal. Permanently disarm before touching wait state:
        // ECHILD can mean an auto-reaper or another owner already released the
        // PID, so Drop must never retry a numeric identifier.
        let mut child = self
            .child
            .take()
            .expect("supervised child presence checked before group signal");
        // A normal-exit zombie rejects or ignores this direct fallback without
        // changing its recorded status. A still-running timeout/cancel target
        // receives the same SIGKILL even if group signalling raced ESRCH.
        let _ = child.kill();
        loop {
            match child.wait() {
                Ok(status) => return Ok(status),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(unix)]
    fn signal_group(&self) {
        // SAFETY: getpgrp has no preconditions and only reads process state.
        if self.process_group > 1 && self.process_group != unsafe { libc::getpgrp() } {
            // SAFETY: spawn() made the child the leader of this dedicated
            // group, and its PID is retained until after this call.
            let _ = unsafe { libc::kill(-self.process_group, libc::SIGKILL) };
        }
    }

    #[cfg(test)]
    pub(crate) fn reap_after_group_kill_with_hook(
        &mut self,
        hook: impl FnOnce(i32),
    ) -> io::Result<ExitStatus> {
        self.reap_after_group_kill_with(hook)
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if self.child.is_none() {
            return;
        }
        if let Err(error) = self.reap_after_group_kill() {
            // reap_after_group_kill permanently disarms after its one group
            // signal, including ECHILD/auto-reap failures. Never retry a
            // numeric PGID whose leader may now have been recycled.
            log::error!("failed to synchronously reap supervised helper: {error}");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::{Read, Write};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    const AUTO_REAP_PROBE_ENV: &str = "JTERM_CORE_SUPERVISED_AUTO_REAP_PROBE";

    extern "C" fn non_reaping_sigchld_handler(_signal: libc::c_int) {}

    fn wait_until_exited(child: &mut SupervisedChild) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if child.root_has_exited().expect("observe child") {
                return;
            }
            assert!(Instant::now() < deadline, "child did not exit in time");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn assert_reaped(pid: i32) {
        let mut status = 0;
        // SAFETY: status is live and pid names the exact child the test owned.
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        assert_eq!(waited, -1, "child {pid} remained waitable after return");
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    fn wait_until_not_live(pid: i32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while crate::process::process_stat(pid).is_some_and(|stat| stat.is_live()) {
            assert!(
                Instant::now() < deadline,
                "process-group descendant {pid} survived cleanup"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn wnowait_keeps_root_reserved_until_group_signal_then_reaps_synchronously() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "exit 23"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = SupervisedChild::spawn(&mut command).unwrap();
        let pid = child.id() as i32;
        wait_until_exited(&mut child);

        let status = child
            .reap_after_group_kill_with_hook(|group| {
                let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
                // SAFETY: info is writable and WNOWAIT observes without reap.
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        group as libc::id_t,
                        &mut info,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                assert_eq!(result, 0);
                assert_eq!(unsafe { info.si_pid() }, group);
            })
            .unwrap();

        assert_eq!(status.code(), Some(23));
        assert_reaped(pid);

        let hook_called = Cell::new(false);
        let error = child
            .reap_after_group_kill_with_hook(|_| hook_called.set(true))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!hook_called.get(), "double reap reached the signal hook");
    }

    #[test]
    fn cached_portable_status_disarms_before_any_signal_or_kill() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "exit 31"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = SupervisedChild::spawn(&mut command).unwrap();
        let pid = child.id() as i32;
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            match child.child.as_mut().unwrap().try_wait().unwrap() {
                Some(status) => break status,
                None => {
                    assert!(Instant::now() < deadline, "child did not exit");
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        };
        child.cached_status = Some(status);
        assert_reaped(pid);

        let hook_called = Cell::new(false);
        let returned = child
            .reap_after_group_kill_with_hook(|_| hook_called.set(true))
            .unwrap();
        assert_eq!(returned.code(), Some(31));
        assert!(!hook_called.get(), "cached status reached the signal hook");
        assert!(child.child.is_none(), "cached status did not disarm child");
    }

    fn root_exit_cleans_background_member(script: &str) {
        let mut command = Command::new("sh");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = SupervisedChild::spawn(&mut command).unwrap();
        let root = child.id() as i32;
        let mut stdout = child.take_stdout().unwrap();
        wait_until_exited(&mut child);
        let status = child.reap_after_group_kill().unwrap();
        assert!(status.success());

        let mut text = String::new();
        stdout.read_to_string(&mut text).unwrap();
        let descendant = text.trim().parse::<i32>().unwrap();
        wait_until_not_live(descendant);
        assert_reaped(root);
    }

    #[test]
    fn normal_root_exit_cleans_background_member_holding_pipe() {
        root_exit_cleans_background_member("sleep 30 & printf '%s\\n' \"$!\"");
    }

    #[test]
    fn normal_root_exit_cleans_background_member_not_holding_pipe() {
        root_exit_cleans_background_member("sleep 30 >/dev/null 2>&1 & printf '%s\\n' \"$!\"");
    }

    #[test]
    fn armed_drop_kills_and_synchronously_reaps_running_root() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "exec sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = SupervisedChild::spawn(&mut command).unwrap();
        let pid = child.id() as i32;
        drop(child);
        assert_reaped(pid);
    }

    #[test]
    fn auto_reap_wait_error_permanently_disarms_before_return() {
        if std::env::var_os(AUTO_REAP_PROBE_ENV).is_none() {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "supervised::tests::auto_reap_wait_error_permanently_disarms_before_return",
                ])
                .env(AUTO_REAP_PROBE_ENV, "1")
                .status()
                .unwrap();
            assert!(status.success(), "isolated auto-reap probe failed");
            return;
        }

        let mut custom_action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        custom_action.sa_sigaction = non_reaping_sigchld_handler as *const () as usize;
        // SAFETY: this exact-test subprocess has no concurrent child owners;
        // the handler intentionally does not reap any status.
        unsafe {
            libc::sigemptyset(&mut custom_action.sa_mask);
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, &custom_action, std::ptr::null_mut()),
                0
            );
        }
        let mut custom_command = Command::new("sh");
        custom_command
            .args(["-c", "exit 19"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut custom_child = SupervisedChild::spawn(&mut custom_command).unwrap();
        wait_until_exited(&mut custom_child);
        assert_eq!(
            custom_child.reap_after_group_kill().unwrap().code(),
            Some(19)
        );

        let mut default_action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        default_action.sa_sigaction = libc::SIG_DFL;
        // SAFETY: restore the ordinary waitable disposition before the
        // auto-reap half of this isolated probe spawns its gated child.
        unsafe {
            libc::sigemptyset(&mut default_action.sa_mask);
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, &default_action, std::ptr::null_mut()),
                0
            );
        }

        let mut command = Command::new("sh");
        command
            .args(["-c", "read release; exit 0"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = SupervisedChild::spawn(&mut command).unwrap();
        let pid = child.id() as i32;
        let mut stdin = child.take_stdin().unwrap();

        // This probe runs in an --exact subprocess so changing SIGCHLD cannot
        // steal statuses from the rest of the parallel test suite. Spawn was
        // deliberately completed under the default disposition first.
        let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        action.sa_sigaction = libc::SIG_IGN;
        // SAFETY: action is writable and then installed for this isolated
        // process only. SIG_IGN makes Linux auto-reap exited children.
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()),
                0
            );
        }
        writeln!(stdin, "release").unwrap();
        drop(stdin);

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            // SAFETY: signal 0 only probes the exact former child pid.
            let result = unsafe { libc::kill(pid, 0) };
            if result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(Instant::now() < deadline, "child was not auto-reaped");
            std::thread::sleep(Duration::from_millis(2));
        }

        let hook_called = Cell::new(false);
        let error = child
            .reap_after_group_kill_with_hook(|_| hook_called.set(true))
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
        assert!(!hook_called.get(), "auto-reaped child reached signal hook");
        assert!(child.child.is_none(), "ECHILD did not permanently disarm");

        let error = child
            .reap_after_group_kill_with_hook(|_| hook_called.set(true))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!hook_called.get(), "disarmed retry reached the signal hook");

        let mut rejected = Command::new("sh");
        rejected
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let error = match SupervisedChild::spawn(&mut rejected) {
            Ok(_) => panic!("SIGCHLD=SIG_IGN spawn unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);

        let mut no_cldwait_action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        no_cldwait_action.sa_sigaction = libc::SIG_DFL;
        no_cldwait_action.sa_flags = libc::SA_NOCLDWAIT;
        // SAFETY: this remains confined to the isolated exact-test process.
        unsafe {
            libc::sigemptyset(&mut no_cldwait_action.sa_mask);
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, &no_cldwait_action, std::ptr::null_mut()),
                0
            );
        }
        let mut rejected = Command::new("sh");
        rejected
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let error = match SupervisedChild::spawn(&mut rejected) {
            Ok(_) => panic!("SA_NOCLDWAIT spawn unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }
}
