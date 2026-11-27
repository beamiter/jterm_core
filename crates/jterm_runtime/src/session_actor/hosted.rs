//! Process and lease boundaries for the Session Host's shared PTY actor.

use super::{ReadOutcome, SessionActor, SessionTransport, SplitWriteLimits, WriteOutcome};
use crate::{
    pty_io::{resize_master_with_pixels, write_master},
    pty_reader::read_master,
    runtime_wake::WakeSink,
    supervised::SupervisedChild,
};
use std::{
    io,
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::process::ExitStatusExt,
    },
    time::{Duration, Instant},
};

pub struct HostedPty {
    master: OwnedFd,
    child: SupervisedChild,
    exit: Option<Result<(i32, bool), String>>,
    output_chunk_bytes: usize,
}

impl SessionActor<HostedPty> {
    /// Adopt the only native master and child wait from confirmed startup.
    pub fn start_hosted(
        master: OwnedFd,
        child: SupervisedChild,
        limits: SplitWriteLimits,
        reply_entries: usize,
        output_chunk_bytes: usize,
        wake: WakeSink,
    ) -> Result<Self, String> {
        Self::start_with_reserved_replies(
            HostedPty {
                master,
                child,
                exit: None,
                output_chunk_bytes,
            },
            wake,
            limits,
            reply_entries,
        )
    }

    /// Retire input and signal the exact owned group. The shared reader stays
    /// active and retains exclusive wait ownership through exit publication.
    pub fn signal_host_stop(&self, force: bool) -> Result<(), String> {
        if !force {
            // Give the in-flight writer a bounded attempt to close any
            // bracketed paste before TERM. Pending messages are discarded.
            self.write_tx
                .queue
                .close_for_host_stop(Duration::from_millis(75));
        }
        let mut transport = self.pty.lock();
        self.close_input();
        if transport.exit.is_some() {
            return Ok(());
        }
        let result = if force {
            transport.child.force_group_exit()
        } else {
            transport.child.terminate_group()
        };
        result.map_err(|error| error.to_string())
    }

    pub fn hosted_cleanup_verified(&self) -> bool {
        self.inspect_transport(|transport| matches!(transport.exit, Some(Ok((_, true)))))
    }

    pub fn hosted_exit_error(&self) -> Option<String> {
        self.inspect_transport(|transport| match &transport.exit {
            Some(Err(error)) => Some(error.clone()),
            _ => None,
        })
    }
}

impl HostedPty {
    fn finish_exit(&mut self) {
        if self.exit.is_some() {
            return;
        }
        let group = self.child.process_group_id();
        self.exit = Some(
            self.child
                .reap_after_group_kill()
                .map(|status| {
                    let code = status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
                    (code, wait_group_empty(group))
                })
                .map_err(|error| format!("Could not stop provider group: {error}")),
        );
    }
}

impl AsFd for HostedPty {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }
}

impl SessionTransport for HostedPty {
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
        let cols = u16::try_from(cols).map_err(|_| "PTY column count out of bounds")?;
        let rows = u16::try_from(rows).map_err(|_| "PTY row count out of bounds")?;
        resize_master_with_pixels(self.as_fd(), cols, rows, pixels)
            .map_err(|error| error.to_string())
    }

    fn is_alive(&mut self) -> bool {
        self.exit.is_none() && matches!(self.child.root_has_exited(), Ok(false))
    }

    fn try_reap(&mut self) -> Option<i32> {
        self.exit_after_tail().ok().flatten()
    }

    fn wait_timeout(&mut self, timeout_ms: u64) -> Result<i32, String> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if self.exit.is_none() && self.child.root_has_exited().map_err(|e| e.to_string())? {
                self.finish_exit();
            }
            if let Some(code) = self.exit_after_tail()? {
                return Ok(code);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("PTY child is still running".into());
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }

    fn observe_before_reap_pid(&self) -> Option<i32> {
        Some(self.child.process_group_id())
    }

    fn read_batch_bytes(&self) -> Option<usize> {
        Some(self.output_chunk_bytes)
    }

    fn max_input_queue_wait(&self) -> Option<Duration> {
        Some(Duration::from_secs(10))
    }

    fn prepare_exit_tail(&mut self) {
        self.finish_exit();
    }

    fn exit_after_tail(&mut self) -> Result<Option<i32>, String> {
        match &self.exit {
            Some(Ok((code, _))) => Ok(Some(*code)),
            Some(Err(error)) => Err(error.clone()),
            None => Ok(None),
        }
    }

    fn terminate_on_read_error(&mut self) -> bool {
        self.finish_exit();
        true
    }

    fn handoff_cleanup(&mut self) -> bool {
        // Registration failure or an abandoned owner must close the child
        // before the caller can release its inherited leases and grants.
        self.finish_exit();
        true
    }

    fn signal_terminate(&mut self) {
        let _ = self.child.terminate_group();
    }

    fn force_kill_and_reap(&mut self) {
        self.finish_exit();
    }
}

fn wait_group_empty(group: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // SAFETY: signal zero checks group liveness without sending a signal.
        if unsafe { libc::kill(-group, 0) } < 0
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}
