use super::*;
use crate::{
    child_lifecycle::ChildExitOutcome,
    pty_reader::{read_master, ReadOutcome},
};
use std::{cell::Cell, convert::Infallible, fs::File, os::fd::AsFd, time::Instant};

fn launch_test_shell(
    executable: &CStr,
    script: &CStr,
    directory: WorkingDirectory<'_>,
) -> Result<PendingPty, PtyLaunchError> {
    launch_pty(PtyLaunch {
        executable,
        argv: &[
            c"launch-probe".to_owned(),
            c"-c".to_owned(),
            script.to_owned(),
        ],
        environment: &[c"PROBE=expected".to_owned()],
        directory,
        rows: 37,
        columns: 101,
        parent_death_signal: true,
    })
}

fn assert_reaped(pid: libc::pid_t) {
    // SAFETY: WNOHANG makes this only a probe of our direct child.
    assert_eq!(
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[cfg(feature = "session-actor")]
#[test]
fn shared_master_write_and_resize_reach_the_real_pty_slave() {
    use crate::{
        pty_io::{resize_master, write_native_master, PtyWriteAttempt},
        session_actor::WriteOutcome,
    };
    use std::io::Read;

    let (master, slave) = pty_pair(24, 80).unwrap();
    resize_master(master.as_fd(), 101, 37).unwrap();
    let mut size = initial_geometry(0, 0);
    // SAFETY: the slave is owned and size is writable.
    assert_eq!(
        unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCGWINSZ, &mut size) },
        0
    );
    assert_eq!((size.ws_row, size.ws_col), (37, 101));
    assert_eq!((size.ws_xpixel, size.ws_ypixel), (0, 0));

    let input = b"input\n";
    assert_eq!(
        write_native_master(master.as_fd(), input, PtyWriteAttempt::run).unwrap(),
        WriteOutcome::Written(input.len())
    );
    let mut events = libc::pollfd {
        fd: slave.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: events is writable and holds the live slave's descriptor.
    assert_eq!(unsafe { libc::poll(&mut events, 1, 1000) }, 1);
    assert_eq!(events.revents & libc::POLLIN, libc::POLLIN);
    let mut received = [0; 32];
    let count = File::from(slave).read(&mut received).unwrap();
    assert_eq!(&received[..count], input);
}

#[test]
fn launches_controlling_tty_with_exact_settings_and_preserves_exit_127() {
    let cwd = File::open("/tmp").unwrap();
    let pending = launch_test_shell(
        c"/bin/sh",
        c"set -e; test -t 0 && test -t 1 && test -t 2 || exit 42; : </dev/tty; printf '%s:%s:%s\\n' \"$0\" \"$PROBE\" \"$PWD\"; exit 127",
        WorkingDirectory::Descriptor(cwd.as_fd()),
    )
    .unwrap();
    let (master, pid) = pending
        .adopt(|master, pid| Ok::<_, Infallible>((master, pid)))
        .unwrap();
    let mut cleanup = PendingChild(Some(pid));

    let mut size = initial_geometry(0, 0);
    // SAFETY: the master is owned and size is writable.
    assert_eq!(
        unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGWINSZ, &mut size) },
        0
    );
    assert_eq!((size.ws_row, size.ws_col), (37, 101));

    let started = Instant::now();
    let mut output = Vec::new();
    let mut buffer = [0; 256];
    loop {
        assert!(started.elapsed() < Duration::from_secs(2));
        match read_master(master.as_fd(), &mut buffer).unwrap() {
            ReadOutcome::Data(count) => output.extend_from_slice(&buffer[..count]),
            ReadOutcome::WouldBlock => std::thread::sleep(Duration::from_millis(1)),
            ReadOutcome::Eof | ReadOutcome::Hangup => break,
        }
    }
    assert_eq!(output, b"launch-probe:expected:/tmp\r\n");
    assert_eq!(
        observe_child_exit(pid, false).unwrap(),
        Some(ChildExitOutcome::Exited(127))
    );
    let mut status = 0;
    // SAFETY: adoption gave this test the exact wait for pid.
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    cleanup.0 = None;
    assert_reaped(pid);
}

#[test]
fn launch_reports_failed_cwd_and_execve_with_their_exact_stage() {
    for (executable, directory, expected) in [
        (
            c"/bin/sh",
            WorkingDirectory::Path(c"/dev/null/missing"),
            StartupStage::WorkingDirectory,
        ),
        (
            c"/dev/null/missing",
            WorkingDirectory::Inherit,
            StartupStage::Exec,
        ),
    ] {
        let Err(failure) = launch_test_shell(executable, c"exit 127", directory) else {
            panic!("unexpected successful PTY launch");
        };
        assert_eq!(
            failure.failed_working_directory(),
            expected == StartupStage::WorkingDirectory
        );
        assert!(matches!(
            failure,
            PtyLaunchError::Start(StartupError::Child { stage, error })
                if stage == expected && error.raw_os_error() == Some(libc::ENOTDIR)
        ));
    }
    assert!(
        !PtyLaunchError::ExecConfirmed(io::Error::other("registration failed"))
            .failed_working_directory()
    );
    assert!(!PtyLaunchError::Start(StartupError::ExecUnconfirmed).failed_working_directory());
}

#[test]
fn failed_transport_adoption_reaps_the_exec_confirmed_child() {
    let pending =
        launch_test_shell(c"/bin/sh", c"exec /bin/sleep 60", WorkingDirectory::Inherit).unwrap();
    let pid = Cell::new(0);
    let result = pending.adopt(|_, child_pid| {
        pid.set(child_pid);
        Err::<(), _>(io::Error::other("could not bind transport"))
    });
    assert!(result.is_err());
    assert!(pid.get() > 0);
    assert_reaped(pid.get());
}

#[test]
fn real_launch_closes_the_registered_flock_even_without_cloexec() {
    const FIXTURE: &str = "JTERM_TEST_PTY_LAUNCH_FLOCK";
    if std::env::var_os(FIXTURE).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_pty_launch::tests::real_launch_closes_the_registered_flock_even_without_cloexec",
            ])
            .env(FIXTURE, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    // SAFETY: memfd_create returns an owned descriptor on success.
    let fd = unsafe { libc::memfd_create(c"launch-flock-test".as_ptr(), 0) };
    assert!(fd > libc::STDERR_FILENO);
    let file = unsafe { File::from_raw_fd(fd) };
    let replacement = File::open(format!("/proc/self/fd/{fd}")).unwrap();
    // SAFETY: both files hold independent descriptions of the same inode.
    assert_eq!(unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let owner = crate::pty_instance_lock::InstanceLock::register(file).unwrap();
    let pending =
        launch_test_shell(c"/bin/sh", c"exec /bin/sleep 60", WorkingDirectory::Inherit).unwrap();
    drop(owner);

    let retry = unsafe { libc::flock(replacement.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(retry, 0, "the live child inherited the flock");
    drop(pending);
}

#[test]
fn partial_status_failure_and_timeout_both_reap_the_startup_child() {
    for pause in [false, true] {
        let (master, slave) = pty_pair(24, 80).unwrap();
        let (reader, writer) = startup_status::channel().unwrap();
        let status_fd = writer.as_raw_fd();
        // SAFETY: the child uses only raw syscalls after fork.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", io::Error::last_os_error());
        if pid == 0 {
            unsafe {
                // The paused fixture may inherit another test's private pipe.
                for fd in 3..1024 {
                    if fd != status_fd {
                        libc::close(fd);
                    }
                }
                libc::write(status_fd, b"E".as_ptr().cast(), 1);
                if pause {
                    loop {
                        libc::pause();
                    }
                }
                libc::_exit(126);
            }
        }
        let pending = PendingPty {
            master,
            child: PendingChild(Some(pid)),
        };
        drop(slave);
        drop(writer);
        let mut pollfd = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one writable pollfd. Keep its byte for confirm to consume.
        assert_eq!(unsafe { libc::poll(&mut pollfd, 1, 2000) }, 1);
        let started = Instant::now();
        let timeout = if pause {
            Duration::from_millis(40)
        } else {
            Duration::from_secs(2)
        };
        let result = pending.confirm(reader, timeout);
        assert!(if pause {
            matches!(
                result,
                Err(PtyLaunchError::Start(StartupError::TimedOut {
                    received: 1,
                    ..
                }))
            )
        } else {
            matches!(
                result,
                Err(PtyLaunchError::Start(StartupError::IncompleteRecord {
                    received: 1
                }))
            )
        });
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_reaped(pid);
    }
}
