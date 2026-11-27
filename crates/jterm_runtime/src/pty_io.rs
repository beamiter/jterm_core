//! Nonblocking writes and window size updates on an owned session's PTY.

use crate::session_actor::WriteOutcome;
use std::{
    io,
    os::fd::{AsRawFd, BorrowedFd},
};

/// Bounded EINTR retry. Zero progress leaves all bytes with the caller.
pub(crate) fn bounded_write(
    input: &[u8],
    mut write: impl FnMut(&[u8]) -> io::Result<usize>,
) -> io::Result<usize> {
    for _ in 0..8 {
        match write(input) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(0),
            Ok(0) if !input.is_empty() => return Err(io::ErrorKind::WriteZero.into()),
            result => return result,
        }
    }
    Ok(0)
}

pub(crate) fn write_master(fd: BorrowedFd<'_>, bytes: &[u8]) -> io::Result<WriteOutcome> {
    let written = bounded_write(bytes, |bytes| PtyWriteAttempt::new(fd, bytes).run())?;
    if written == 0 {
        Ok(WriteOutcome::WouldBlock)
    } else {
        Ok(WriteOutcome::Written(written))
    }
}

/// One syscall on a nonblocking master. A host can run the attempt under its
/// input-admission fence without keeping that fence across interrupted retries.
pub struct PtyWriteAttempt<'a> {
    master: BorrowedFd<'a>,
    bytes: &'a [u8],
}

impl<'a> PtyWriteAttempt<'a> {
    pub fn new(master: BorrowedFd<'a>, bytes: &'a [u8]) -> Self {
        Self { master, bytes }
    }

    pub fn run(self) -> io::Result<usize> {
        // SAFETY: the borrow pins the master and the slice is readable.
        let result = unsafe {
            libc::write(
                self.master.as_raw_fd(),
                self.bytes.as_ptr().cast(),
                self.bytes.len(),
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    }
}

/// Native transport compatibility policy. Interruption attempts consume no
/// bytes and reacquire the host fence. A successful count, including zero,
/// ends the operation and leaves any unwritten suffix with the session actor.
pub fn write_native_master<'a>(
    master: BorrowedFd<'a>,
    bytes: &'a [u8],
    mut attempt: impl FnMut(PtyWriteAttempt<'a>) -> io::Result<usize>,
) -> io::Result<WriteOutcome> {
    loop {
        match attempt(PtyWriteAttempt::new(master, bytes)) {
            Ok(count) => return Ok(WriteOutcome::Written(count)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Ok(WriteOutcome::WouldBlock);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Install a cell size after the caller applies its profile's dimension policy.
pub fn resize_master(fd: BorrowedFd<'_>, cols: u16, rows: u16) -> io::Result<()> {
    resize_master_with_pixels(fd, cols, rows, (0, 0))
}

pub(crate) fn resize_master_with_pixels(
    fd: BorrowedFd<'_>,
    cols: u16,
    rows: u16,
    pixels: (u16, u16),
) -> io::Result<()> {
    let size = crate::native_pty_launch::pty_window_size(rows, cols, pixels);
    // SAFETY: the borrow pins the master and size is initialized.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &size) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::File, os::fd::AsFd};

    #[test]
    fn native_master_retries_unchanged_input_through_fence_and_stops_on_partial_write() {
        let master = File::open("/dev/null").unwrap();
        let mut admissions = 0;
        let result = write_native_master(master.as_fd(), b"input", |attempt| {
            assert_eq!(attempt.bytes, b"input");
            admissions += 1;
            if admissions <= 10 {
                Err(io::ErrorKind::Interrupted.into())
            } else {
                Ok(2)
            }
        });
        assert_eq!(result.unwrap(), WriteOutcome::Written(2));
        assert_eq!(admissions, 11);
    }

    #[test]
    fn native_master_keeps_zero_and_backpressure_policies_and_stops_on_fence_rejection() {
        let master = File::open("/dev/null").unwrap();
        let success = write_native_master(master.as_fd(), b"input", |_| Ok(0));
        assert_eq!(success.unwrap(), WriteOutcome::Written(0));
        let blocked = write_native_master(master.as_fd(), b"input", |_| {
            Err(io::ErrorKind::WouldBlock.into())
        });
        assert_eq!(blocked.unwrap(), WriteOutcome::WouldBlock);
        let mut admissions = 0;
        let rejected = write_native_master(master.as_fd(), b"input", |_| {
            admissions += 1;
            Err(io::Error::other("native input rejected"))
        });
        assert_eq!(rejected.unwrap_err().to_string(), "native input rejected");
        assert_eq!(admissions, 1);
    }
}
