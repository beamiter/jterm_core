//! One-shot Linux shell-integration identity transport.

use crate::pty_reader::cloexec_pipe_above_stdio;
use std::fmt::Write as _;
use std::io;
use std::os::fd::OwnedFd;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

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
