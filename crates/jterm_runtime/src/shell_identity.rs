//! One-shot Linux shell-integration identity transport.

use std::fmt::Write as _;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// A private token and its pipe, prepared before forking a shell. Both ends
/// begin close-on-exec. Only the child may admit the read end across exec.
pub struct ShellIdentityChannel {
    token: String,
    read_end: OwnedFd,
    write_end: OwnedFd,
}

impl ShellIdentityChannel {
    pub fn allocate() -> io::Result<Option<Self>> {
        let mut random = [0_u8; 16];
        // SAFETY: `random` is writable for the exact supplied length.
        let read = unsafe {
            libc::getrandom(
                random.as_mut_ptr().cast(),
                random.len(),
                libc::GRND_NONBLOCK,
            )
        };
        if read != random.len() as isize {
            // Unavailable entropy disables verified execution for this shell.
            return Ok(None);
        }
        let mut token = String::with_capacity(random.len() * 2);
        for byte in random {
            let _ = write!(token, "{byte:02x}");
        }

        let (read_end, write_end) = cloexec_pipe_above_stdio()?;
        Ok(Some(Self {
            token,
            read_end,
            write_end,
        }))
    }

    /// Transfer the token, child read end and parent write end to the spawner.
    pub fn into_parts(self) -> (String, OwnedFd, OwnedFd) {
        (self.token, self.read_end, self.write_end)
    }
}

pub(super) fn cloexec_pipe_above_stdio() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1; 2];
    // SAFETY: the array provides space for both new descriptor numbers.
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe2` returned two fresh, owned descriptors. Own both
    // before attempting the fallible remap above stdin/stdout/stderr.
    let (read_end, write_end) = unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    };
    Ok((move_above_stdio(read_end)?, move_above_stdio(write_end)?))
}

pub(super) fn move_above_stdio(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() > libc::STDERR_FILENO {
        return Ok(fd);
    }
    // SAFETY: `fd` pins the source, and fcntl returns a fresh CLOEXEC duplicate.
    let duplicate = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the duplicate is now owned by this process.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{Read, Write};

    #[test]
    fn token_and_both_cloexec_descriptors_form_one_bounded_channel() {
        let channel = ShellIdentityChannel::allocate().unwrap().unwrap();
        let (token, reader, writer) = channel.into_parts();
        assert_eq!(token.len(), 32);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        for descriptor in [&reader, &writer] {
            let fd = descriptor.as_raw_fd();
            assert!(fd > libc::STDERR_FILENO);
            // SAFETY: `descriptor` owns this live fd.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(flags >= 0);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }

        let payload = format!("{token}\n");
        let mut writer = File::from(writer);
        writer.write_all(payload.as_bytes()).unwrap();
        drop(writer);
        let mut delivered = String::new();
        File::from(reader).read_to_string(&mut delivered).unwrap();
        assert_eq!(delivered, payload);
    }
}
