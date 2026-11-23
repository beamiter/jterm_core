//! Linux transport for the standalone profile's shared session actor.
use super::{bounded_write, reaper, ChildLifecycle};
use crate::pty_reader::read_master;
use crate::session_actor::{ReadOutcome, SessionTransport, WriteOutcome};
use portable_pty::{MasterPty, PtySize};
use std::{
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
    sync::Arc,
    time::{Duration, Instant},
};

pub(super) struct Transport {
    master: Master,
    child: Option<Arc<ChildLifecycle>>,
    lease: Option<reaper::Lease>,
}

enum Master {
    Portable {
        owner: Box<dyn MasterPty + Send>,
        fd: RawFd,
    },
    Native(OwnedFd),
}

impl Transport {
    pub(super) fn new(
        master: Box<dyn MasterPty + Send>,
        fd: RawFd,
        child: Arc<ChildLifecycle>,
        lease: reaper::Lease,
    ) -> Self {
        Self {
            master: Master::Portable { owner: master, fd },
            child: Some(child),
            lease: Some(lease),
        }
    }

    pub(super) fn from_native(
        master: OwnedFd,
        child: Arc<ChildLifecycle>,
        lease: reaper::Lease,
    ) -> Self {
        Self {
            master: Master::Native(master),
            child: Some(child),
            lease: Some(lease),
        }
    }

    #[cfg(test)]
    pub(super) fn size(&self) -> PtySize {
        match &self.master {
            Master::Portable { owner, .. } => owner.get_size().unwrap(),
            Master::Native(master) => {
                let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
                assert_eq!(
                    unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGWINSZ, size.as_mut_ptr()) },
                    0
                );
                let size = unsafe { size.assume_init() };
                PtySize {
                    rows: size.ws_row,
                    cols: size.ws_col,
                    pixel_width: size.ws_xpixel,
                    pixel_height: size.ws_ypixel,
                }
            }
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        if matches!(&self.master, Master::Native(_)) {
            // Actor setup can fail after the startup wait owner handed off.
            self.handoff_cleanup();
        }
    }
}

impl AsFd for Transport {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match &self.master {
            // The owner stays alive for this borrow of the transport.
            Master::Portable { fd, .. } => unsafe { BorrowedFd::borrow_raw(*fd) },
            Master::Native(master) => master.as_fd(),
        }
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
        match &self.master {
            Master::Portable { owner, .. } => owner
                .resize(PtySize {
                    rows: rows as u16,
                    cols: cols as u16,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|error| error.to_string()),
            Master::Native(master) => resize_master(master.as_fd(), cols as u16, rows as u16)
                .map_err(|error| error.to_string()),
        }
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
