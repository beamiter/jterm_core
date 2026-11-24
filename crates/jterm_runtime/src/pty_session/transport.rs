//! Linux transport for the standalone profile's shared session actor.
use super::{bounded_write, reaper, ChildLifecycle};
use crate::pty_reader::read_master;
use crate::session_actor::{ReadOutcome, SessionTransport, WriteOutcome};
use std::{
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
    sync::Arc,
    time::{Duration, Instant},
};

pub(super) struct Transport {
    master: OwnedFd,
    child: Option<Arc<ChildLifecycle>>,
    lease: Option<reaper::Lease>,
}

impl Transport {
    pub(super) fn new(master: OwnedFd, child: Arc<ChildLifecycle>, lease: reaper::Lease) -> Self {
        Self {
            master,
            child: Some(child),
            lease: Some(lease),
        }
    }

    #[cfg(test)]
    pub(super) fn size(&self) -> libc::winsize {
        let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
        // SAFETY: the master is live and ioctl writes one winsize on success.
        assert_eq!(
            unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCGWINSZ, size.as_mut_ptr()) },
            0
        );
        unsafe { size.assume_init() }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        // Actor setup can fail after the startup wait owner handed off.
        self.handoff_cleanup();
    }
}

impl AsFd for Transport {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }
}

impl SessionTransport for Transport {
    fn read(&mut self, buffer: &mut [u8]) -> Result<ReadOutcome, String> {
        read_master(self.as_fd(), buffer).map_err(|error| error.to_string())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<WriteOutcome, String> {
        write_master(self.as_fd(), bytes).map_err(|error| error.to_string())
    }

    fn resize(&mut self, cols: usize, rows: usize) -> Result<(), String> {
        resize_master(self.as_fd(), cols as u16, rows as u16).map_err(|error| error.to_string())
    }

    fn is_alive(&mut self) -> bool {
        self.child
            .as_ref()
            .is_some_and(|child| child.exit_outcome().is_none())
    }

    fn try_reap(&mut self) -> Option<i32> {
        self.child.as_ref()?.poll_reap()
    }

    fn wait_timeout(&mut self, timeout_ms: u64) -> Result<i32, String> {
        let timeout = Duration::from_millis(timeout_ms);
        let start = Instant::now();
        loop {
            if let Some(code) = self.try_reap() {
                return Ok(code);
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err("PTY child is still running".into());
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }

    fn observe_before_reap_pid(&self) -> Option<i32> {
        Some(self.child.as_ref()?.pid())
    }

    fn handoff_cleanup(&mut self) -> bool {
        if let (Some(child), Some(lease)) = (self.child.take(), self.lease.take()) {
            lease.handoff(reaper::Job::Bound(child));
        }
        true
    }

    fn signal_terminate(&mut self) {
        self.handoff_cleanup();
    }

    fn force_kill_and_reap(&mut self) {
        self.handoff_cleanup();
    }
}

pub(super) fn write_master(fd: BorrowedFd<'_>, bytes: &[u8]) -> io::Result<WriteOutcome> {
    let written = bounded_write(bytes, |bytes| {
        // SAFETY: the borrow pins the master and bytes is valid for its length.
        let result = unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    })?;
    if written == 0 {
        Ok(WriteOutcome::WouldBlock)
    } else {
        Ok(WriteOutcome::Written(written))
    }
}

pub(super) fn resize_master(fd: BorrowedFd<'_>, cols: u16, rows: u16) -> io::Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: the borrow pins the master and size is initialized.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &size) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
