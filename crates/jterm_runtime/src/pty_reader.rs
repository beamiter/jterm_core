//! Cancellable Unix master I/O and non-consuming child exit observation.

use crate::child_lifecycle::{observe_child_exit, ChildExitOutcome};
use crate::exit_status::ExitTail;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    Data(usize),
    WouldBlock,
    Eof,
    Hangup,
}

/// Read a nonblocking master without mistaking a temporarily closed slave
/// for the exit of the process that owns it.
pub fn read_master(fd: BorrowedFd<'_>, buffer: &mut [u8]) -> io::Result<ReadOutcome> {
    loop {
        // SAFETY: the descriptor and writable slice live through the syscall.
        let count = unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if count > 0 {
            return Ok(ReadOutcome::Data(count as usize));
        }
        if count == 0 {
            return Ok(ReadOutcome::Eof);
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return Ok(ReadOutcome::WouldBlock),
            _ if error.raw_os_error() == Some(libc::EIO) => return Ok(ReadOutcome::Hangup),
            _ => return Err(error),
        }
    }
}

/// Closing the owned write end wakes the reader. CLOEXEC prevents subsequently
/// launched shells from keeping someone else's cancellation pipe alive.
pub fn shutdown_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    // SAFETY: fds exposes storage for two descriptors.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let result = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pipe creation transferred both descriptors.
    let pipe = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    for fd in [&pipe.0, &pipe.1] {
        // SAFETY: each descriptor is still owned by pipe.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(pipe)
}

pub(crate) fn move_above_stdio(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() > libc::STDERR_FILENO {
        return Ok(fd);
    }
    // SAFETY: `fd` pins the source, and fcntl returns a new CLOEXEC descriptor.
    let duplicate = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

pub(crate) fn cloexec_pipe_above_stdio() -> io::Result<(OwnedFd, OwnedFd)> {
    let (reader, writer) = shutdown_pipe()?;
    Ok((move_above_stdio(reader)?, move_above_stdio(writer)?))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReaderPoll {
    Shutdown,
    Data,
    Timeout,
}

pub(crate) fn poll_reader(fd: RawFd, shutdown: RawFd, timeout: i32) -> io::Result<ReaderPoll> {
    poll_with(fd, shutdown, timeout, Instant::now, |fds, timeout| {
        // SAFETY: poll receives a live array of two initialized records.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
        if ready < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ready)
        }
    })
}

fn poll_with(
    fd: RawFd,
    shutdown: RawFd,
    timeout: i32,
    mut now: impl FnMut() -> Instant,
    mut poll: impl FnMut(&mut [libc::pollfd; 2], i32) -> io::Result<i32>,
) -> io::Result<ReaderPoll> {
    let budget = (timeout >= 0).then(|| (now(), Duration::from_millis(timeout as u64)));
    let mut remaining = timeout;
    let mut fds = [fd, shutdown].map(|fd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    });
    loop {
        match poll(&mut fds, remaining) {
            Ok(0) => return Ok(ReaderPoll::Timeout),
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                if let Some((start, budget)) = budget {
                    let elapsed = now().duration_since(start);
                    if elapsed >= budget {
                        return Ok(ReaderPoll::Timeout);
                    }
                    remaining = (budget - elapsed)
                        .as_nanos()
                        .div_ceil(1_000_000)
                        .min(i32::MAX as u128) as i32;
                }
            }
            Err(error) => return Err(error),
        }
    }
    const READY: i16 = libc::POLLIN | libc::POLLHUP | libc::POLLERR;
    if fds[1].revents & (READY | libc::POLLNVAL) != 0 || fds[0].revents & libc::POLLNVAL != 0 {
        return Ok(ReaderPoll::Shutdown);
    }
    Ok(if fds[0].revents & READY != 0 {
        ReaderPoll::Data
    } else {
        ReaderPoll::Timeout
    })
}

#[derive(Debug)]
pub enum PtyReadEvent {
    Output {
        bytes: Vec<u8>,
        read_at: Instant,
    },
    Exit {
        outcome: ChildExitOutcome,
        tail: ExitTail,
    },
    Error(io::Error),
}

const BATCH_CAP: usize = 1024 * 1024;
pub(crate) const EXIT_TAIL_BYTES: usize = 1024 * 1024;
const PROBE_MS: i32 = 200;

/// Drain the PTY bytes already readable after the leader has exited. Read in
/// 32 KiB chunks for existing application-owned PTYs, including blocking masters.
/// None means cancellation or consumer loss. The caller still owns child reaping.
pub fn drain_ready_exit_tail(
    master: BorrowedFd<'_>,
    mut emit: impl FnMut(Vec<u8>) -> bool,
    cancelled: impl FnMut() -> bool,
) -> Option<ExitTail> {
    let mut buffer = [0; 32 * 1024];
    drain_exit_tail(
        &mut buffer,
        |buffer| match poll_reader(master.as_raw_fd(), -1, 0)? {
            ReaderPoll::Data => read_master(master, buffer),
            ReaderPoll::Timeout => Ok(ReadOutcome::WouldBlock),
            ReaderPoll::Shutdown => Err(io::Error::other("PTY master is unavailable")),
        },
        |bytes, _| emit(bytes.to_vec()),
        cancelled,
    )
}

pub(crate) fn drain_exit_tail<E: std::fmt::Display>(
    buffer: &mut [u8],
    mut read: impl FnMut(&mut [u8]) -> Result<ReadOutcome, E>,
    mut emit: impl FnMut(&[u8], Instant) -> bool,
    mut cancelled: impl FnMut() -> bool,
) -> Option<ExitTail> {
    let mut remaining = EXIT_TAIL_BYTES;
    while !cancelled() {
        if remaining == 0 {
            return Some(ExitTail::LimitReached);
        }
        let available = buffer.len().min(remaining);
        match read(&mut buffer[..available]) {
            Ok(ReadOutcome::Data(count)) if (1..=available).contains(&count) => {
                remaining -= count;
                if !emit(&buffer[..count], Instant::now()) {
                    return None;
                }
            }
            Ok(ReadOutcome::WouldBlock | ReadOutcome::Eof | ReadOutcome::Hangup) => {
                return Some(ExitTail::Complete);
            }
            Ok(ReadOutcome::Data(count)) => {
                log::warn!("Invalid PTY exit read length: {count} for a {available}-byte buffer");
                return Some(ExitTail::ReadFailed);
            }
            Err(error) => {
                log::warn!("Could not finish reading PTY exit output: {error}");
                return Some(ExitTail::ReadFailed);
            }
        }
    }
    None
}

fn observed_exit(child_pid: i32) -> Option<ChildExitOutcome> {
    match observe_child_exit(child_pid, true) {
        Ok(outcome) => outcome,
        Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
            Some(ChildExitOutcome::Unavailable)
        }
        Err(error) => {
            log::warn!("could not observe PTY child exit: {error}");
            None
        }
    }
}

/// Run the ordered producer for a nonblocking master. Only the session owner
/// can reap the child. A bounded tail is published before the observed exit,
/// including when a descendant still holds the slave open. The consumer
/// returns false to cancel if delivery is no longer possible.
pub fn run(
    master: BorrowedFd<'_>,
    shutdown: BorrowedFd<'_>,
    child_pid: i32,
    send: impl FnMut(PtyReadEvent) -> bool,
) {
    run_observed(
        master,
        shutdown,
        child_pid,
        ReadLimits::default(),
        || {},
        send,
    );
}

#[derive(Default)]
pub(crate) struct ReadLimits {
    pub hangup_timeout: Option<Duration>,
    pub batch_bytes: Option<usize>,
}

pub(crate) fn run_observed(
    master: BorrowedFd<'_>,
    shutdown: BorrowedFd<'_>,
    child_pid: i32,
    limits: ReadLimits,
    mut retire_input: impl FnMut(),
    mut send: impl FnMut(PtyReadEvent) -> bool,
) {
    let batch_bytes = limits.batch_bytes.unwrap_or(BATCH_CAP).clamp(1, BATCH_CAP);
    let mut buffer = vec![0; batch_bytes.min(65536)];
    let mut probe_at = Instant::now();
    let mut hangup_delay = None;
    let mut hangup_started = None;
    loop {
        if Instant::now() >= probe_at || hangup_delay.is_some() {
            match poll_reader(-1, shutdown.as_raw_fd(), 0) {
                Ok(ReaderPoll::Shutdown) => return,
                Ok(_) => {}
                Err(error) => {
                    send(PtyReadEvent::Error(error));
                    return;
                }
            }
            if let Some(exit) = observed_exit(child_pid) {
                retire_input();
                let tail = drain_exit_tail(
                    &mut buffer,
                    |buffer| read_master(master, buffer),
                    |bytes, read_at| {
                        send(PtyReadEvent::Output {
                            bytes: bytes.to_vec(),
                            read_at,
                        })
                    },
                    || {
                        matches!(
                            poll_reader(-1, shutdown.as_raw_fd(), 0),
                            Ok(ReaderPoll::Shutdown)
                        )
                    },
                );
                if let Some(tail) = tail {
                    send(PtyReadEvent::Exit {
                        outcome: exit,
                        tail,
                    });
                }
                return;
            }
            probe_at = Instant::now() + Duration::from_millis(PROBE_MS as u64);
        }

        if let Some(delay) = hangup_delay {
            if hangup_started
                .zip(limits.hangup_timeout)
                .is_some_and(|(started, limit): (Instant, Duration)| started.elapsed() >= limit)
            {
                send(PtyReadEvent::Error(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "PTY hung up while its child was still running",
                )));
                return;
            }
            // A hung-up master remains readable. Wait only on cancellation
            // before trying the slave again, so the worker cannot busy-spin.
            match poll_reader(-1, shutdown.as_raw_fd(), delay) {
                Ok(ReaderPoll::Shutdown) => return,
                Ok(_) => {}
                Err(error) => {
                    send(PtyReadEvent::Error(error));
                    return;
                }
            }
        } else {
            match poll_reader(master.as_raw_fd(), shutdown.as_raw_fd(), PROBE_MS) {
                Ok(ReaderPoll::Shutdown) => return,
                Ok(ReaderPoll::Timeout) => continue,
                Ok(ReaderPoll::Data) => {}
                Err(error) => {
                    send(PtyReadEvent::Error(error));
                    return;
                }
            }
        }

        match publish_ready(master, &mut buffer, batch_bytes, &mut send) {
            Some(true) => {
                hangup_started.get_or_insert_with(Instant::now);
                hangup_delay = Some(hangup_delay.map_or(50, |delay| (delay * 2).min(250)));
            }
            Some(false) => {
                hangup_started = None;
                hangup_delay = None;
            }
            None => return,
        }
    }
}

/// None means the consumer disconnected or the stream failed. The bool
/// distinguishes slave hangup from ordinary batch completion.
fn publish_ready(
    master: BorrowedFd<'_>,
    buffer: &mut [u8],
    batch_bytes: usize,
    send: &mut impl FnMut(PtyReadEvent) -> bool,
) -> Option<bool> {
    let mut bytes = Vec::new();
    let mut read_at = None;
    let mut hangup = false;
    let mut error = None;
    while bytes.len() < batch_bytes {
        let available = buffer.len().min(batch_bytes - bytes.len());
        match read_master(master, &mut buffer[..available]) {
            Ok(ReadOutcome::Data(count)) => {
                read_at.get_or_insert_with(Instant::now);
                bytes.extend_from_slice(&buffer[..count]);
            }
            Ok(ReadOutcome::WouldBlock) => break,
            Ok(ReadOutcome::Eof | ReadOutcome::Hangup) => {
                hangup = true;
                break;
            }
            Err(cause) => {
                error = Some(cause);
                break;
            }
        }
    }
    if let Some(read_at) = read_at {
        if !send(PtyReadEvent::Output { bytes, read_at }) {
            return None;
        }
    }
    if let Some(error) = error {
        send(PtyReadEvent::Error(error));
        return None;
    }
    Some(hangup)
}

#[cfg(test)]
mod reader_poll_tests {
    use super::*;
    use std::cell::Cell;
    use std::io::Write;
    use std::os::fd::AsFd;

    #[test]
    fn blocking_exit_tail_drains_only_bytes_already_ready() {
        let (read, write) = shutdown_pipe().unwrap();
        let mut write = std::fs::File::from(write);
        write.write_all(b"exit tail").unwrap();
        let mut captured = Vec::new();
        let result = drain_ready_exit_tail(
            read.as_fd(),
            |chunk| {
                captured.extend(chunk);
                true
            },
            || false,
        );
        assert_eq!(result, Some(ExitTail::Complete));
        assert_eq!(captured, b"exit tail");
        drop(write);
    }

    fn interrupted() -> std::io::Result<i32> {
        Err(std::io::Error::from_raw_os_error(libc::EINTR))
    }

    #[test]
    fn reader_poll_100_interruptions_share_one_50ms_budget() {
        let start = Instant::now();
        let ticks = Cell::new(0);
        let mut waits = Vec::new();
        let result = poll_with(
            -1,
            -1,
            50,
            || start + Duration::from_micros(ticks.get() * 500),
            |_, ms| {
                waits.push(ms);
                ticks.set(ticks.get() + 1);
                assert!(ticks.get() <= 100, "EINTR reset the original budget");
                interrupted()
            },
        )
        .unwrap();
        assert!(matches!(result, ReaderPoll::Timeout));
        assert_eq!(waits.len(), 100);
        assert_eq!(&waits[..4], &[50, 50, 49, 49]);
        assert_eq!(waits.last(), Some(&1));
        assert!(waits.iter().all(|&ms| ms > 0));
    }

    #[test]
    fn reader_poll_zero_still_polls_once_and_does_not_spin_on_eintr() {
        let mut calls = 0;
        let result = poll_with(-1, -1, 0, Instant::now, |_, ms| {
            calls += 1;
            assert_eq!(calls, 1);
            assert_eq!(ms, 0);
            interrupted()
        })
        .unwrap();
        assert!(matches!(result, ReaderPoll::Timeout));
    }

    #[test]
    fn reader_poll_negative_waits_retry_without_reading_clock() {
        for timeout in [-1, -17, i32::MIN] {
            let mut calls = 0;
            let result = poll_with(
                -1,
                -1,
                timeout,
                || panic!("infinite wait must not read the clock"),
                |fds, ms| {
                    assert_eq!(ms, timeout);
                    calls += 1;
                    if calls <= 100 {
                        interrupted()
                    } else {
                        fds[1].revents = libc::POLLHUP;
                        Ok(1)
                    }
                },
            )
            .unwrap();
            assert!(matches!(result, ReaderPoll::Shutdown));
            assert_eq!(calls, 101);
        }
    }

    #[test]
    fn reader_poll_rounds_positive_remainder_up_and_preserves_ready_result() {
        let start = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let mut waits = Vec::new();
        let result = poll_with(
            -1,
            -1,
            50,
            || start + elapsed.get(),
            |fds, ms| {
                waits.push(ms);
                if waits.len() == 1 {
                    elapsed.set(Duration::from_micros(49_999));
                    interrupted()
                } else {
                    assert_eq!(waits.len(), 2);
                    elapsed.set(Duration::from_secs(1));
                    fds[0].revents = libc::POLLIN;
                    fds[1].revents = libc::POLLHUP;
                    Ok(2)
                }
            },
        )
        .unwrap();
        assert_eq!(waits, [50, 1]);
        assert!(matches!(result, ReaderPoll::Shutdown));
    }

    #[test]
    fn reader_poll_non_eintr_errors_are_not_timeout_or_eof() {
        let error = match poll_with(-1, -1, 50, Instant::now, |_, _| {
            Err(std::io::Error::from_raw_os_error(libc::EINVAL))
        }) {
            Err(error) => error,
            Ok(_) => panic!("poll failure must propagate"),
        };
        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
    }

    fn pipe() -> (OwnedFd, OwnedFd) {
        shutdown_pipe().unwrap()
    }

    fn write_byte(fd: &OwnedFd) {
        let byte = b'x';
        // SAFETY: fd is live and the byte is valid for this write.
        assert_eq!(
            unsafe { libc::write(fd.as_raw_fd(), (&byte as *const u8).cast(), 1) },
            1
        );
    }

    #[test]
    fn reader_poll_kernel_idle_blocks_and_timeout_does_not_consume_data() {
        let (data, writer) = pipe();
        let (shutdown, _keep_open) = pipe();
        let start = Instant::now();
        assert!(matches!(
            poll_reader(data.as_raw_fd(), shutdown.as_raw_fd(), 50).unwrap(),
            ReaderPoll::Timeout
        ));
        assert!(
            start.elapsed() >= Duration::from_millis(40),
            "idle poll must block"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "idle wait exceeded budget tolerance"
        );
        write_byte(&writer);
        assert!(matches!(
            poll_reader(data.as_raw_fd(), shutdown.as_raw_fd(), 0).unwrap(),
            ReaderPoll::Data
        ));
        let mut byte = 0u8;
        assert_eq!(
            unsafe { libc::read(data.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) },
            1
        );
        assert_eq!(byte, b'x');
        drop(writer);
        // A concurrent PTY launch can briefly inherit the writer before exec.
        assert!(
            matches!(
                poll_reader(data.as_raw_fd(), shutdown.as_raw_fd(), 1000).unwrap(),
                ReaderPoll::Data
            ),
            "HUP remains readable EOF, not timeout/shutdown"
        );
        assert_eq!(
            unsafe { libc::read(data.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) },
            0
        );
    }

    #[test]
    fn reader_poll_kernel_finite_and_infinite_waits_are_cancellable() {
        for timeout in [5000, -1] {
            let (data, _writer) = pipe();
            let (shutdown, close_to_cancel) = pipe();
            std::thread::scope(|scope| {
                let cancel = scope.spawn(move || {
                    std::thread::sleep(Duration::from_millis(20));
                    drop(close_to_cancel);
                });
                let start = Instant::now();
                assert!(matches!(
                    poll_reader(data.as_raw_fd(), shutdown.as_raw_fd(), timeout).unwrap(),
                    ReaderPoll::Shutdown
                ));
                assert!(start.elapsed() < Duration::from_secs(2));
                cancel.join().unwrap();
            });
        }
    }

    #[test]
    fn reader_poll_kernel_shutdown_wins_simultaneous_readiness() {
        let (data, writer) = pipe();
        let (shutdown, cancel) = pipe();
        write_byte(&writer);
        write_byte(&cancel);
        assert!(matches!(
            poll_reader(data.as_raw_fd(), shutdown.as_raw_fd(), 0).unwrap(),
            ReaderPoll::Shutdown
        ));
        drop(cancel);
        assert!(matches!(
            poll_reader(data.as_raw_fd(), shutdown.as_raw_fd(), 0).unwrap(),
            ReaderPoll::Shutdown
        ));
    }

    #[test]
    fn reader_poll_kernel_invalid_fd_is_shutdown() {
        let (shutdown, _writer) = pipe();
        // F_DUPFD cannot allocate at RLIMIT_NOFILE; using the limit itself
        // is an invalid positive fd without racing descriptor reuse.
        let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) },
            0
        );
        let limit = unsafe { limit.assume_init() }.rlim_cur;
        let invalid = i32::try_from(limit).unwrap_or(i32::MAX);
        assert!(matches!(
            poll_reader(invalid, shutdown.as_raw_fd(), 0).unwrap(),
            ReaderPoll::Shutdown
        ));
    }

    #[test]
    fn reader_poll_kernel_real_pty_data_and_hangup_remain_data() {
        let (mut master, mut slave) = (-1, -1);
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        // SAFETY: successful openpty transfers ownership of these descriptors.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        // Avoid retaining the slave across unrelated concurrent execs,
        // which would postpone the hangup this test is about to observe.
        for fd in [&master, &slave] {
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0);
            assert_eq!(
                unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) },
                0
            );
        }
        let (shutdown, _keep_open) = pipe();
        write_byte(&slave);
        assert!(matches!(
            poll_reader(master.as_raw_fd(), shutdown.as_raw_fd(), 1000).unwrap(),
            ReaderPoll::Data
        ));
        let mut byte = 0u8;
        assert_eq!(
            unsafe { libc::read(master.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) },
            1
        );
        assert_eq!(byte, b'x');
        drop(slave);
        assert!(matches!(
            poll_reader(master.as_raw_fd(), shutdown.as_raw_fd(), 1000).unwrap(),
            ReaderPoll::Data
        ));
    }
}
