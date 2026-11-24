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

pub(crate) fn resize_master(fd: BorrowedFd<'_>, cols: u16, rows: u16) -> io::Result<()> {
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
