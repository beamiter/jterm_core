//! A session whose native host prepared the child and PTY master.

use crate::child_lifecycle::{observe_child_exit, ChildLifecycle, EscalationPolicy};
use crate::pty_io::{resize_master, resize_master_with_pixels, write_master};
use crate::pty_reader::read_master;
use crate::runtime_wake::WakeSink;
use crate::session_actor::{
    ReadOutcome, SessionActor, SessionEvent, SessionTransport, SplitWriteLimits, WriteError,
    WriteOutcome,
};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::Duration;

/// Takes exclusive ownership of I/O on a prepared master. The host still
/// supplies its verified pre-reap session cleanup and escalation policy.
pub struct PreparedPty {
    actor: SessionActor<Transport>,
}

impl PreparedPty {
    pub fn start(
        master: OwnedFd,
        child: Arc<ChildLifecycle>,
        policy: EscalationPolicy,
        poll_reap: impl Fn(&ChildLifecycle) -> Option<i32> + Send + 'static,
        limits: SplitWriteLimits,
        reply_entries: usize,
        wake: WakeSink,
    ) -> io::Result<Self> {
        let transport = Transport {
            master,
            child,
            policy,
            poll_reap: Box::new(poll_reap),
        };
        let fd = transport.as_fd().as_raw_fd();
        // SAFETY: transport owns the live descriptor and no other I/O
        // worker has started. The flags also apply to the actor's poll fd.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let actor =
            SessionActor::start_with_reserved_replies(transport, wake, limits, reply_entries)
                .map_err(io::Error::other)?;
        Ok(Self { actor })
    }

    pub fn next_event(&self) -> Option<SessionEvent> {
        self.actor.events().try_recv().ok()
    }

    pub fn write_input(&self, bytes: Vec<u8>) -> Result<(), WriteError> {
        self.actor.write_sender().try_send(bytes)
    }

    pub fn write_reply(&self, bytes: Vec<u8>) -> Result<(), WriteError> {
        self.actor.write_response(bytes)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.actor
            .try_inspect_transport(|transport| {
                if self.actor.is_stopped() || transport.child.exit_code().is_some() {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                resize_master(transport.as_fd(), cols, rows)
            })
            .unwrap_or_else(|| Err(io::ErrorKind::WouldBlock.into()))
    }

    /// Borrow the master for a synchronous read-only host probe.
    pub fn inspect_fd<R>(&self, inspect: impl FnOnce(RawFd) -> R) -> Option<R> {
        self.actor
            .try_inspect_transport(|transport| inspect(transport.as_fd().as_raw_fd()))
    }

    pub fn foreground_is_codex(&self) -> bool {
        self.actor
            .try_inspect_transport(|transport| {
                let fd = transport.as_fd().as_raw_fd();
                super::foreground_is_codex(transport.child.pid(), || {
                    // SAFETY: the actor retains the PTY master for this probe.
                    let group = unsafe { libc::tcgetpgrp(fd) };
                    (group > 0).then_some(group)
                })
            })
            .unwrap_or(false)
    }

    pub fn input_stamp(&self) -> Option<u64> {
        self.actor.display_idle_input_stamp()
    }

    pub fn try_write_control(&self, byte: u8, validate: impl FnOnce(i32) -> bool) -> bool {
        self.actor.try_write_control_byte(byte, validate)
    }

    pub fn freeze_after_exit(&self) {
        self.actor.freeze_after_exit();
    }

    pub fn stop(&self) {
        self.actor.stop();
    }
}

type PollReap = dyn Fn(&ChildLifecycle) -> Option<i32> + Send;

struct Transport {
    master: OwnedFd,
    child: Arc<ChildLifecycle>,
    policy: EscalationPolicy,
    poll_reap: Box<PollReap>,
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
        self.resize_with_pixels(cols, rows, (0, 0))
    }

    fn resize_with_pixels(
        &mut self,
        cols: usize,
        rows: usize,
        pixels: (u16, u16),
    ) -> Result<(), String> {
        resize_master_with_pixels(self.as_fd(), cols as u16, rows as u16, pixels)
            .map_err(|error| error.to_string())
    }

    fn is_alive(&mut self) -> bool {
        self.child.exit_outcome().is_none()
    }

    fn try_reap(&mut self) -> Option<i32> {
        self.child
            .exit_code()
            .or_else(|| (self.poll_reap)(&self.child))
    }

    fn wait_timeout(&mut self, timeout_ms: u64) -> Result<i32, String> {
        let start = std::time::Instant::now();
        let limit = Duration::from_millis(timeout_ms);
        loop {
            if let Some(code) = self.try_reap() {
                return Ok(code);
            }
            let remaining = limit.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err("PTY child is still running".into());
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }

    fn observe_before_reap_pid(&self) -> Option<i32> {
        Some(self.child.pid())
    }

    fn hangup_timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(5))
    }

    fn read_batch_bytes(&self) -> Option<usize> {
        Some(32 * 1024)
    }

    fn control_input_available(&mut self) -> bool {
        self.is_alive() && matches!(observe_child_exit(self.child.pid(), true), Ok(None))
    }

    fn terminate_on_read_error(&mut self) -> bool {
        self.signal_terminate();
        true
    }

    fn handoff_cleanup(&mut self) -> bool {
        self.signal_terminate();
        true
    }

    fn signal_terminate(&mut self) {
        self.child.terminate(self.policy);
    }

    fn force_kill_and_reap(&mut self) {
        self.child.force_kill_and_reap();
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.signal_terminate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::child_lifecycle::ReapOwner;
    use crate::native_pty_launch::{launch_bundled_bash, BundledBash};
    use std::path::Path;
    use std::time::Instant;

    #[test]
    fn adopted_master_orders_input_and_reply_and_drains_real_exit_output() {
        let bash = Path::new("/bin/bash");
        if !bash.is_file() {
            return;
        }
        let pending = launch_bundled_bash(BundledBash {
            executable: bash,
            startup_script: b"stty -echo; printf 'prepared-ready\\n'; IFS= read -r input; printf 'size=%s data=%s\\n' \"$(stty size)\" \"$input\"; head -c 131072 /dev/zero | tr '\\000' x; printf 'prepared-tail\\n'; exit 23\n",
            cwd: Path::new("/"),
            environment: &[("PATH".into(), "/usr/bin:/bin".into())],
            identity_fd_key: "JTERM_TEST_IDENTITY_FD",
            identity_token_key: "JTERM_TEST_IDENTITY_TOKEN",
            rows: 24,
            columns: 80,
        })
        .unwrap();
        let (master, child) = pending
            .adopt(|master, pid| {
                ChildLifecycle::new(pid, ReapOwner::Ours).map(|child| (master, child))
            })
            .unwrap();
        let pty = PreparedPty::start(
            master,
            child.clone(),
            EscalationPolicy::SESSION_DRAIN,
            ChildLifecycle::poll_reap,
            SplitWriteLimits {
                user_bytes: 64,
                response_bytes: 64,
                entries: 4,
                coalesce_bytes: 0,
            },
            1,
            WakeSink::new(|| {}),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut output = Vec::new();
        let mut input_sent = false;
        let exit = loop {
            assert!(Instant::now() < deadline, "prepared PTY did not exit");
            match pty.next_event() {
                Some(SessionEvent::Output(batch)) => {
                    assert!(batch.len() <= 32 * 1024);
                    assert!(batch.fragments().all(|fragment| fragment.origin.is_some()));
                    output.extend_from_slice(batch.as_bytes());
                }
                Some(SessionEvent::Exit(code)) => break code,
                Some(event) => panic!("unexpected event: {event:?}"),
                None => std::thread::sleep(Duration::from_millis(2)),
            }
            if !input_sent && output.ends_with(b"prepared-ready\r\n") {
                pty.resize(91, 35).unwrap();
                pty.write_input(b"input-".to_vec()).unwrap();
                pty.write_reply(b"reply\n".to_vec()).unwrap();
                input_sent = true;
            }
        };
        assert_eq!(exit, 23);
        assert_eq!(child.exit_code(), Some(exit));
        let receipt = b"size=35 91 data=input-reply";
        assert!(output
            .windows(receipt.len())
            .any(|window| window == receipt));
        assert!(output.ends_with(b"prepared-tail\r\n"));
        assert_eq!(output.iter().filter(|&&byte| byte == b'x').count(), 131072);
        assert_eq!(pty.write_input(vec![b'x']), Err(WriteError::Closed));
        pty.freeze_after_exit();
    }
}
