#![cfg(all(target_os = "linux", feature = "linux-pty"))]

use jterm_runtime::pty_session::Session;
use std::{
    mem::MaybeUninit,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, AtomicI32, Ordering},
    thread,
    time::{Duration, Instant},
};

static WATCHED_PID: AtomicI32 = AtomicI32::new(0);
static WATCHED_REAPED: AtomicBool = AtomicBool::new(false);

extern "C" fn watch_child(_: libc::c_int) {
    let pid = WATCHED_PID.load(Ordering::Relaxed);
    if pid > 0 {
        // Preserve errno on whichever thread received the signal. This watcher
        // claims just the child it registered, as a GUI child watch would.
        unsafe {
            let errno = libc::__errno_location();
            let saved = *errno;
            if libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) == pid {
                WATCHED_PID.store(0, Ordering::Relaxed);
                WATCHED_REAPED.store(true, Ordering::Release);
            }
            *errno = saved;
        }
    }
}

struct RestoreAction(libc::sigaction);

impl Drop for RestoreAction {
    fn drop(&mut self) {
        unsafe { libc::sigaction(libc::SIGCHLD, &self.0, std::ptr::null_mut()) };
    }
}

fn install_action(handler: libc::sighandler_t, flags: libc::c_int) -> RestoreAction {
    let mut previous = MaybeUninit::uninit();
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler;
        action.sa_flags = flags;
        assert_eq!(libc::sigemptyset(&mut action.sa_mask), 0);
        assert_eq!(
            libc::sigaction(libc::SIGCHLD, &action, previous.as_mut_ptr()),
            0
        );
        RestoreAction(previous.assume_init())
    }
}

fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

// This is the only test in this process because SIGCHLD is process-wide.
#[test]
fn separate_child_watches_leave_pty_exit_status_with_its_owner() {
    let handler = watch_child as *const () as libc::sighandler_t;
    {
        let _restore = install_action(handler, libc::SA_RESTART | libc::SA_NOCLDSTOP);
        let mut watched = Command::new("/bin/sh")
            .args(["-c", "read release"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        WATCHED_PID.store(watched.id().try_into().unwrap(), Ordering::Relaxed);

        let mut session = Session::spawn(6, 72).unwrap();
        drop(watched.stdin.take());
        session
            .submit_input(b"printf 'PTY_%s\\n' WAIT_OK; exit 7\r".to_vec())
            .unwrap();
        until("PTY child to exit and drain", || {
            session.drive().unwrap();
            session.is_drained()
        });
        assert_eq!(session.exit_code(), Some(7));
        assert!(session.terminal().visible_text().contains("PTY_WAIT_OK"));
        session.close().unwrap();
        until("PTY child cleanup", || !session.cleanup_pending());
        until("the registered child to be reaped", || {
            WATCHED_REAPED.load(Ordering::Acquire)
        });
        assert_eq!(
            watched.try_wait().unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    for (handler, flags) in [(libc::SIG_IGN, 0), (handler, libc::SA_NOCLDWAIT)] {
        let _restore = install_action(handler, flags);
        assert_eq!(
            Session::spawn(6, 72).err().as_deref(),
            Some("child supervision requires a waitable SIGCHLD disposition"),
        );
    }
}
