//! In-process Linux PTY session for the shared standalone terminal profile.
//! Drives bounded reads and replies, owns the direct child and its deferred
//! reaping, and keeps exit detection separate from output EOF.
use crate::child_lifecycle::{
    observe_child_exit, ChildExitOutcome, ChildLifecycle, EscalationPolicy, ProcessRef,
};
use crate::exit_status::{ExitTail, PtyExit};
use crate::native_pty_launch::{
    launch_bundled_bash, launch_standalone_pty, BundledBash, ForwardedFd, PendingPty, PtyLaunch,
    PtyLaunchError, WorkingDirectory,
};
use crate::process_input_budget::{shared_budget, Charge};
#[cfg(test)]
use crate::pty_io::bounded_write;
use crate::pty_reader::move_above_stdio;
use crate::runtime_wake::WakeSink;
use crate::session_actor::{SessionActor, SessionEvent};
use crate::supervised::require_waitable_sigchld;
use crate::terminal::Emulator;
use crossbeam_channel::TryRecvError;
use jterm_engine::frame::CellFrame;
use jterm_engine::terminal::{
    Terminal, MAX_FEED_BYTES as READ_BUDGET, MAX_INPUT_BYTES as MAX_INPUT, MAX_STREAMED_PASTE_BYTES,
};
use std::{
    collections::VecDeque,
    ffi::OsString,
    io::{self, Cursor, Read},
    num::NonZeroU64,
    os::fd::{AsFd, BorrowedFd, RawFd},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

mod input_queue;
mod prepared;
mod program;
mod transport;
use input_queue::InputQueue;
pub use prepared::PreparedPty;
use program::Program;
use transport::Transport;

const INPUT_BOUNDARY_TIMEOUT: Duration = Duration::from_secs(2);
const BOUNDARY_STALLED: &str = "logical input boundary stalled; input disabled until shell closes";
const BOUNDARY_RETIRED: &str =
    "logical input boundary interrupted by view change; input disabled until shell closes";
const BOUNDARY_FAILED: &str =
    "terminal stopped during logical input boundary; input disabled until shell closes";

#[derive(Clone, Copy)]
enum InputBoundary {
    Queued(Instant),
    Raw { start: Instant, remaining: usize },
}

impl InputBoundary {
    fn started(self) -> Instant {
        match self {
            Self::Queued(start) | Self::Raw { start, .. } => start,
        }
    }
}

mod reaper {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    };
    pub(super) const CAPACITY: usize = 32;
    #[derive(Default)]
    pub(super) struct Ticket {
        pub reaped: AtomicBool,
        pub pending: AtomicBool,
    }
    pub(super) enum Job {
        Bound(Arc<ChildLifecycle>),
        #[cfg(test)]
        Probe(Box<dyn FnMut() -> io::Result<bool> + Send>),
    }

    impl Job {
        fn tick(&mut self) -> bool {
            match self {
                Self::Bound(child) => {
                    if child.exit_outcome().is_none() {
                        // Only start group cleanup while the exact wait owner
                        // still holds the kernel's PID/session anchor.
                        match observe_child_exit(child.pid(), true) {
                            Ok(_) => {
                                child.terminate(EscalationPolicy::SESSION_DRAIN);
                            }
                            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                                child.poll_reap_outcome();
                            }
                            Err(_) => {}
                        }
                    }
                    matches!(
                        child.exit_outcome(),
                        Some(ChildExitOutcome::Exited(_) | ChildExitOutcome::Signaled(_))
                    )
                }
                #[cfg(test)]
                Self::Probe(probe) => probe().unwrap_or(false),
            }
        }
    }

    struct OwnedChild {
        job: Job,
        ticket: Arc<Ticket>,
    }
    enum Slot {
        Free,
        Reserved,
        Pending(OwnedChild),
        Processing,
    }
    pub(super) struct Reaper {
        slots: Mutex<Vec<Slot>>,
        healthy: AtomicBool,
    }
    pub(super) struct Lease {
        owner: Arc<Reaper>,
        index: usize,
        active: bool,
        pub ticket: Arc<Ticket>,
    }
    impl Reaper {
        fn table() -> Arc<Self> {
            Arc::new(Self {
                slots: Mutex::new((0..CAPACITY).map(|_| Slot::Free).collect()),
                healthy: AtomicBool::new(true),
            })
        }
        pub fn reserve(self: &Arc<Self>) -> Result<Lease, String> {
            if !self.healthy.load(Ordering::Acquire) {
                return Err("reaper unavailable".into());
            }
            let mut slots = self.slots.lock().map_err(|_| "reaper poisoned")?;
            if !self.healthy.load(Ordering::Acquire) {
                return Err("reaper unavailable".into());
            }
            let index = slots
                .iter()
                .position(|slot| matches!(slot, Slot::Free))
                .ok_or("session capacity exhausted")?;
            slots[index] = Slot::Reserved;
            Ok(Lease {
                owner: self.clone(),
                index,
                active: true,
                ticket: Arc::new(Ticket::default()),
            })
        }
        fn tick(&self) {
            if !self.healthy.load(Ordering::Acquire) {
                return;
            }
            for index in 0..CAPACITY {
                let mut owned = {
                    let Ok(mut slots) = self.slots.lock() else {
                        self.healthy.store(false, Ordering::Release);
                        return;
                    };
                    if !matches!(slots[index], Slot::Pending(_)) {
                        continue;
                    }
                    match std::mem::replace(&mut slots[index], Slot::Processing) {
                        Slot::Pending(child) => child,
                        _ => unreachable!(),
                    }
                };
                // No mutex spans OS calls. Retain ownership on errors/panics.
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owned.job.tick()));
                let reaped = matches!(result, Ok(true));
                if result.is_err() {
                    self.healthy.store(false, Ordering::Release);
                }
                match self.slots.lock() {
                    Ok(mut slots) => {
                        if reaped {
                            owned.ticket.reaped.store(true, Ordering::Release);
                            owned.ticket.pending.store(false, Ordering::Release);
                            slots[index] = Slot::Free;
                        } else {
                            slots[index] = Slot::Pending(owned);
                        }
                    }
                    Err(poison) => {
                        self.healthy.store(false, Ordering::Release);
                        // Preserve owner/slot even if the table is poisoned; never free it.
                        poison.into_inner()[index] = Slot::Pending(owned);
                        return;
                    }
                }
                if !self.healthy.load(Ordering::Acquire) {
                    return;
                }
            }
        }
    }
    impl Lease {
        pub fn handoff(mut self, job: Job) {
            self.ticket.pending.store(true, Ordering::Release);
            let owned = OwnedChild {
                job,
                ticket: self.ticket.clone(),
            };
            match self.owner.slots.lock() {
                Ok(mut slots) => slots[self.index] = Slot::Pending(owned),
                Err(poison) => {
                    self.owner.healthy.store(false, Ordering::Release);
                    poison.into_inner()[self.index] = Slot::Pending(owned);
                }
            }
            self.active = false;
        }
    }
    impl Drop for Lease {
        fn drop(&mut self) {
            if self.active {
                if let Ok(mut slots) = self.owner.slots.lock() {
                    if matches!(slots[self.index], Slot::Reserved) {
                        slots[self.index] = Slot::Free;
                    }
                }
            }
        }
    }
    fn initialize(
        start_worker: impl FnOnce(Arc<Reaper>) -> Result<(), String>,
    ) -> Result<Arc<Reaper>, String> {
        // No child exists yet. A failed worker/capability probe cannot orphan one.
        let probe = ProcessRef::open(std::process::id() as i32).map_err(|e| e.to_string())?;
        if !probe.is_kernel_bound() {
            return Err("kernel-bound child reference unavailable".into());
        }
        probe.signal(0).map_err(|e| e.to_string())?;
        let reaper = Reaper::table();
        start_worker(reaper.clone())?;
        Ok(reaper)
    }
    pub(super) fn global() -> Result<Arc<Reaper>, String> {
        static GLOBAL: OnceLock<Result<Arc<Reaper>, String>> = OnceLock::new();
        GLOBAL
            .get_or_init(|| {
                initialize(|worker| {
                    std::thread::Builder::new()
                        .name("jterm-child-reaper".into())
                        .spawn(move || loop {
                            worker.tick();
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        })
                        .map_err(|e| format!("reaper startup: {e}"))?;
                    Ok(())
                })
            })
            .clone()
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::AtomicUsize;
        fn probe(state: &Arc<AtomicUsize>) -> Job {
            let state = Arc::clone(state);
            Job::Probe(Box::new(move || match state.load(Ordering::Acquire) {
                0 => Ok(false),
                1 => Err(io::Error::from_raw_os_error(libc::ECHILD)),
                2 => Ok(true),
                _ => panic!("injected child polling panic"),
            }))
        }

        #[test]
        fn startup_failure_publishes_no_reaper_or_child() {
            assert!(initialize(|_| Err("injected worker startup failure".into())).is_err());
        }
        #[test]
        fn poisoned_worker_table_retains_handoff_and_capacity() {
            let owner = Reaper::table();
            let lease = owner.reserve().unwrap();
            let index = lease.index;
            let ticket = lease.ticket.clone();
            let poisoned = owner.clone();
            assert!(std::thread::spawn(move || {
                let _guard = poisoned.slots.lock().unwrap();
                panic!("injected table poison");
            })
            .join()
            .is_err());
            let state = Arc::new(AtomicUsize::new(2));
            lease.handoff(probe(&state));
            owner.tick();
            assert!(owner.reserve().is_err());
            assert!(ticket.pending.load(Ordering::Acquire));
            assert!(!ticket.reaped.load(Ordering::Acquire));
            let slots = match owner.slots.lock() {
                Err(poison) => poison.into_inner(),
                Ok(_) => panic!("expected poison"),
            };
            assert!(matches!(slots[index], Slot::Pending(_)));
        }
        #[test]
        fn permanent_unreaped_children_exhaust_exact_fixed_capacity() {
            let owner = Reaper::table();
            let state = Arc::new(AtomicUsize::new(0));
            for _ in 0..CAPACITY {
                owner.reserve().unwrap().handoff(probe(&state));
            }
            for _ in 0..3 {
                owner.tick();
            }
            assert!(owner.reserve().is_err());
            assert_eq!(owner.slots.lock().unwrap().len(), CAPACITY);
            state.store(2, Ordering::Release);
            owner.tick();
            assert!(owner.reserve().is_ok());
        }
        #[test]
        fn setup_failure_releases_reservation_before_any_child() {
            let owner = Reaper::table();
            fn failed_setup(owner: &Arc<Reaper>) -> Result<(), String> {
                let _lease = owner.reserve()?;
                Err("injected openpty/spawn failure".into())
            }
            for _ in 0..CAPACITY * 2 {
                assert!(failed_setup(&owner).is_err());
            }
            let leases: Vec<_> = (0..CAPACITY).map(|_| owner.reserve().unwrap()).collect();
            assert!(owner.reserve().is_err());
            drop(leases);
            assert!(owner.reserve().is_ok());
        }
        #[test]
        fn wait_error_holds_the_slot_until_confirmed_reap() {
            for initial in [0, 1] {
                let owner = Reaper::table();
                let state = Arc::new(AtomicUsize::new(initial));
                let lease = owner.reserve().unwrap();
                let index = lease.index;
                let ticket = lease.ticket.clone();
                lease.handoff(probe(&state));
                owner.tick();
                {
                    let slots = owner.slots.lock().unwrap();
                    assert!(matches!(
                        &slots[index],
                        Slot::Pending(OwnedChild {
                            job: Job::Probe(_),
                            ..
                        })
                    ));
                }
                state.store(0, Ordering::Release);
                owner.tick();
                assert!(ticket.pending.load(Ordering::Acquire));
                assert!(!ticket.reaped.load(Ordering::Acquire));
                state.store(2, Ordering::Release);
                owner.tick();
                assert!(ticket.reaped.load(Ordering::Acquire));
                let next = owner.reserve().unwrap();
                assert_eq!(next.index, index);
                assert!(!next.ticket.reaped.load(Ordering::Acquire));
                assert!(ticket.reaped.load(Ordering::Acquire));
            }
        }
        #[test]
        fn poll_panic_quarantines_owner_and_rejects_new_spawn() {
            let owner = Reaper::table();
            let state = Arc::new(AtomicUsize::new(3));
            let lease = owner.reserve().unwrap();
            let ticket = lease.ticket.clone();
            lease.handoff(probe(&state));
            owner.tick();
            assert!(owner.reserve().is_err());
            assert!(ticket.pending.load(Ordering::Acquire));
            assert!(!ticket.reaped.load(Ordering::Acquire));
            state.store(2, Ordering::Release);
            owner.tick();
            assert!(ticket.pending.load(Ordering::Acquire));
        }
    }
}

fn installed_jsh() -> Result<std::path::PathBuf, String> {
    std::env::var_os("PATH")
        .and_then(|path| find_installed_jsh(&path))
        .ok_or_else(|| "jsh executable not found on the application PATH".into())
}

fn find_installed_jsh(path: &std::ffi::OsStr) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    std::env::split_paths(path)
        .filter(|entry| entry.is_absolute())
        .filter_map(|entry| entry.join("jsh").canonicalize().ok())
        .find(|executable| {
            executable
                .metadata()
                .is_ok_and(|info| info.is_file() && info.permissions().mode() & 0o111 != 0)
        })
}

struct SessionIo {
    actor: SessionActor<Transport>,
    output: Cursor<Vec<u8>>,
    #[cfg(test)]
    reader: Option<Box<dyn Read + Send>>,
    #[cfg(test)]
    writer: Option<Box<dyn io::Write + Send>>,
    #[cfg(test)]
    write_limit: Option<usize>,
}

impl SessionIo {
    fn write(&mut self, bytes: &[u8]) -> Result<usize, String> {
        #[cfg(test)]
        if let Some(writer) = &mut self.writer {
            return bounded_write(bytes, |bytes| writer.write(bytes))
                .map_err(|error| error.to_string());
        }
        #[cfg(test)]
        let bytes = &bytes[..self.write_limit.unwrap_or(bytes.len()).min(bytes.len())];
        self.actor.try_write_owner_prefix(bytes)
    }

    fn read(
        &mut self,
        buffer: &mut [u8],
        exit_code: &mut Option<i32>,
        exit_tail: &mut ExitTail,
    ) -> io::Result<usize> {
        #[cfg(test)]
        if let Some(reader) = &mut self.reader {
            return reader.read(buffer);
        }
        if self.output.position() as usize == self.output.get_ref().len() {
            self.output = Cursor::default();
            loop {
                match self.actor.events().try_recv() {
                    Ok(SessionEvent::Output(batch)) => {
                        self.output = Cursor::new(batch.into_bytes());
                        break;
                    }
                    Ok(SessionEvent::ExitTail(tail)) => *exit_tail = tail,
                    Ok(SessionEvent::Exit(code)) => {
                        *exit_code = Some(code);
                        return Ok(0);
                    }
                    Ok(SessionEvent::Error(error)) => return Err(io::Error::other(error)),
                    Err(TryRecvError::Empty) => return Err(io::ErrorKind::WouldBlock.into()),
                    Err(TryRecvError::Disconnected) => {
                        return Err(io::Error::other("PTY reader disconnected before exit"));
                    }
                }
            }
        }
        self.output.read(buffer)
    }
}

fn launch_error(error: PtyLaunchError) -> String {
    match error {
        PtyLaunchError::Prepare(error) => error.to_string(),
        error => error.to_string(),
    }
}

/// Owns the direct PTY child and its wait status. Other child watchers in
/// the embedding process must only wait for PIDs they own.
pub struct Session {
    io: Option<SessionIo>,
    output_wake: Arc<std::sync::OnceLock<WakeSink>>,
    child: Arc<ChildLifecycle>,
    reap_ticket: Arc<reaper::Ticket>,
    terminal: Emulator,
    input: InputQueue,
    ordered_input: bool,
    input_boundary: Option<InputBoundary>,
    replies: VecDeque<u8>,
    reply_charge: Charge,
    read_budget_blocked: bool,
    holding_replies: bool,
    exited: bool,
    exit_code: Option<i32>,
    exit_tail: ExitTail,
    closed: bool,
    read_eof: bool,
    write_error: Option<String>,
    dropped_reply_bytes: usize,
}
impl Session {
    pub fn spawn(rows: usize, cols: usize) -> Result<Self, String> {
        Self::spawn_with_epoch(rows, cols, NonZeroU64::MIN)
    }

    pub fn spawn_with_epoch(rows: usize, cols: usize, epoch: NonZeroU64) -> Result<Self, String> {
        let terminal = Terminal::with_epoch(rows, cols, epoch)?;
        let command = Program::interactive(
            "/bin/sh".into(),
            &[("TERM", "dumb"), ("PS1", "$ "), ("ENV", "/dev/null")],
        )?;
        Self::spawn_program(terminal, command.spec(rows as u16, cols as u16), None)
    }

    /// Explicitly launch the local user's installed jsh with output observation.
    /// Executable lookup is limited to absolute entries in the native PATH.
    pub fn spawn_integrated_with_epoch(
        rows: usize,
        cols: usize,
        epoch: NonZeroU64,
    ) -> Result<Self, String> {
        let mut terminal = Terminal::with_epoch(rows, cols, epoch)?;
        let command = Program::interactive(installed_jsh()?, &[("TERM", "xterm")])?;
        terminal.observe_command_blocks();
        Self::spawn_program(terminal, command.spec(rows as u16, cols as u16), None)
    }

    /// Start a shell with the shared emulator's own terminfo and device replies.
    pub fn spawn_family_with_epoch(
        rows: usize,
        cols: usize,
        epoch: NonZeroU64,
        integrated: bool,
    ) -> Result<Self, String> {
        let mut terminal = Terminal::with_epoch(rows, cols, epoch)?;
        terminal.enable_device_answers();
        let mut environment = crate::terminal_profile::environment().to_vec();
        let program = if integrated {
            terminal.observe_command_blocks();
            installed_jsh()?
        } else {
            environment.extend([("PS1", "$ "), ("ENV", "/dev/null")]);
            "/bin/sh".into()
        };
        let command = Program::interactive(program, &environment)?;
        Self::spawn_program(terminal, command.spec(rows as u16, cols as u16), None)
    }

    /// Start one local PTY using the caller's prepared command and environment.
    /// The same owner supplies terminal state, input, resize and child cleanup.
    pub fn spawn_command(
        rows: usize,
        cols: usize,
        argv: &[OsString],
        cwd: Option<&Path>,
        env: &[(OsString, OsString)],
    ) -> Result<Self, String> {
        let terminal = Terminal::new(rows, cols)?;
        if argv.first().is_none_or(|program| program.is_empty()) {
            return Err("missing PTY command".into());
        }
        let cwd = match cwd {
            Some(path) => path
                .canonicalize()
                .map_err(|error| format!("invalid PTY working directory: {error}"))?,
            None => std::env::current_dir()
                .map_err(|error| format!("cannot resolve PTY working directory: {error}"))?,
        };
        if !cwd.is_dir() {
            return Err("PTY working directory is not a directory".into());
        }
        let command = Program::command(argv, &cwd, env)?;
        let mut session =
            Self::spawn_program(terminal, command.spec(rows as u16, cols as u16), None)?;
        session.enable_ordered_input();
        Ok(session)
    }

    /// Start an already resolved program in the exact open task directory.
    /// The child enters the directory and closes the descriptor before exec.
    pub fn spawn_pinned_command(
        rows: usize,
        cols: usize,
        argv: &[OsString],
        directory: BorrowedFd<'_>,
        env: &[(OsString, OsString)],
    ) -> Result<Self, String> {
        if !argv
            .first()
            .is_some_and(|program| Path::new(program).is_absolute())
        {
            return Err("a pinned PTY command requires an absolute executable".into());
        }
        let terminal = Terminal::new(rows, cols)?;
        let command = Program::command(argv, Path::new("/"), env)?;
        let mut spec = command.spec(rows as u16, cols as u16);
        spec.directory = WorkingDirectory::Descriptor(directory);
        let mut session = Self::spawn_program(terminal, spec, None)?;
        session.enable_ordered_input();
        Ok(session)
    }

    /// Start the sandbox-side PTY bridge from `/` with exactly one extra
    /// descriptor at the number its protocol expects. The parent keeps its
    /// original descriptor and flags.
    pub fn spawn_command_with_forwarded_fd(
        rows: usize,
        cols: usize,
        argv: &[OsString],
        directory: BorrowedFd<'_>,
        child_fd: RawFd,
        env: &[(OsString, OsString)],
    ) -> Result<Self, String> {
        let terminal = Terminal::new(rows, cols)?;
        let command = Program::command(argv, Path::new("/"), env)?;
        let forwarded = directory
            .try_clone_to_owned()
            .and_then(move_above_stdio)
            .map_err(|error| format!("could not pin PTY bridge descriptor: {error}"))?;
        let mut session = Self::spawn_program(
            terminal,
            command.spec(rows as u16, cols as u16),
            Some(ForwardedFd {
                source: forwarded.as_fd(),
                child_fd,
            }),
        )?;
        session.enable_ordered_input();
        Ok(session)
    }

    /// Launch the configured local Bash with Anvil's bundled integration.
    pub fn spawn_anvil_bash(
        rows: usize,
        cols: usize,
        executable: &Path,
        startup_script: &[u8],
        cwd: &Path,
        environment: &[(OsString, OsString)],
    ) -> Result<Self, String> {
        let terminal = Terminal::new(rows, cols)?;
        let lease = reaper::global()?.reserve()?;
        let pending = launch_bundled_bash(BundledBash {
            executable,
            startup_script,
            cwd,
            environment,
            identity_fd_key: "ANVIL_SHELL_INTEGRATION_FD",
            identity_token_key: "ANVIL_SHELL_INTEGRATION_TOKEN",
            rows: rows as u16,
            columns: cols as u16,
        })
        .map_err(launch_error)?;
        let mut session = Self::register_program(pending, lease, terminal)?;
        session.enable_ordered_input();
        Ok(session)
    }

    fn spawn_program(
        terminal: Terminal,
        spec: PtyLaunch<'_>,
        forwarded: Option<ForwardedFd<'_>>,
    ) -> Result<Self, String> {
        require_waitable_sigchld().map_err(|error| error.to_string())?;
        let lease = reaper::global()?.reserve()?;
        let pending = launch_standalone_pty(spec, forwarded).map_err(launch_error)?;
        Self::register_program(pending, lease, terminal)
    }

    fn register_program(
        pending: PendingPty,
        lease: reaper::Lease,
        terminal: Terminal,
    ) -> Result<Self, String> {
        let reap_ticket = lease.ticket.clone();
        let (transport, lifecycle) = pending
            .adopt(|master, pid| {
                let child = ChildLifecycle::new_kernel_bound(pid)?;
                Ok::<_, io::Error>((Transport::new(master, child.clone(), lease), child))
            })
            .map_err(|error| PtyLaunchError::ExecConfirmed(error).to_string())?;
        Self::with_transport(transport, lifecycle, reap_ticket, terminal)
            .map_err(|error| format!("shell exec confirmed but session setup failed: {error}"))
    }

    fn with_transport(
        transport: Transport,
        lifecycle: Arc<ChildLifecycle>,
        reap_ticket: Arc<reaper::Ticket>,
        terminal: Terminal,
    ) -> Result<Self, String> {
        let output_wake = Arc::new(std::sync::OnceLock::<WakeSink>::new());
        let ready = output_wake.clone();
        let actor = SessionActor::start_with_owner_writes(
            transport,
            WakeSink::new(move || {
                if let Some(wake) = ready.get() {
                    wake.wake();
                }
            }),
        )?;
        Ok(Self {
            io: Some(SessionIo {
                actor,
                output: Cursor::default(),
                #[cfg(test)]
                reader: None,
                #[cfg(test)]
                writer: None,
                #[cfg(test)]
                write_limit: None,
            }),
            output_wake,
            child: lifecycle,
            reap_ticket,
            terminal: Emulator { engine: terminal },
            input: InputQueue::default(),
            ordered_input: false,
            input_boundary: None,
            replies: VecDeque::new(),
            reply_charge: shared_budget().empty_charge(),
            read_budget_blocked: false,
            holding_replies: false,
            exited: false,
            exit_code: None,
            exit_tail: ExitTail::Complete,
            closed: false,
            read_eof: false,
            write_error: None,
            dropped_reply_bytes: 0,
        })
    }
    pub fn terminal(&self) -> &Terminal {
        &self.terminal.engine
    }

    /// Notify the parser owner when the reader publishes output or exit.
    /// An immediate wake covers events published before registration.
    pub fn set_output_wake(&self, wake: WakeSink) -> Result<(), String> {
        self.output_wake
            .set(wake.clone())
            .map_err(|_| "PTY output wake already registered".to_owned())?;
        wake.wake();
        Ok(())
    }

    /// Apply direct graphics and their replies in the same PTY byte stream.
    pub fn enable_inline_graphics(
        &mut self,
        cell_pixels: (u16, u16),
        decode_png: jterm_engine::terminal::PngDecoder,
    ) {
        self.terminal
            .engine
            .enable_inline_graphics(cell_pixels, decode_png);
    }

    /// An owner-mode writer may need to retry after EAGAIN without new output.
    pub fn needs_write_retry(&self) -> bool {
        !self.closed
            && (self.read_budget_blocked
                || (!self.is_exited()
                    && self.write_error.is_none()
                    && (!self.input_idle() || !self.replies.is_empty())))
    }

    pub fn child_pid(&self) -> Option<i32> {
        if self.closed || self.is_exited() {
            return None;
        }
        Some(self.child.pid())
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    pub fn exit_status(&self) -> Option<PtyExit> {
        self.exit_code.map(|code| PtyExit {
            code,
            tail: self.exit_tail,
        })
    }

    pub fn enable_underline_colors(&mut self) {
        self.terminal.engine.enable_underline_colors();
    }

    pub fn enable_grapheme_clusters(&mut self) {
        self.terminal.engine.enable_grapheme_clusters();
    }

    pub fn enable_hyperlinks(&mut self, policy: jterm_engine::hyperlink::HyperlinkPolicy) {
        self.terminal.engine.enable_hyperlinks(policy);
    }

    pub fn enable_window_updates(&mut self) {
        self.terminal.engine.enable_window_updates();
    }

    pub fn set_color_defaults(&mut self, defaults: jterm_engine::colors::TerminalDefaultColors) {
        self.terminal.engine.set_color_defaults(defaults);
    }

    pub fn enable_keyboard_disambiguation(&mut self) {
        self.terminal.engine.enable_keyboard_disambiguation();
    }

    pub fn set_history_row_limit(&mut self, rows: usize) {
        self.terminal.engine.set_history_row_limit(rows);
    }

    pub fn enable_history_preserving_resize(&mut self) {
        self.terminal.engine.enable_history_preserving_resize();
    }

    pub fn enable_primary_reflow(&mut self) {
        self.terminal.engine.enable_primary_reflow();
    }

    pub fn take_window_updates(&mut self) -> jterm_engine::terminal::WindowUpdates {
        self.terminal.engine.take_window_updates()
    }

    /// A partial logical event keeps its place ahead of later replies and input.
    pub fn enable_ordered_input(&mut self) {
        self.ordered_input = true;
    }

    /// Admit a whole logical user event. The receipt does not claim a kernel write.
    pub fn submit_input(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        self.admit_input()?;
        self.input.enqueue(bytes)
    }

    /// Keep a native bracketed paste as a single ordered record even though
    /// the kernel accepts it in several writes. Ordinary input keeps its budget.
    pub fn submit_native_paste(&mut self, text: &str) -> Result<bool, String> {
        self.admit_input()?;
        if !self.ordered_input {
            return Err("native paste requires ordered input".into());
        }
        let bytes = self.terminal.engine.encode_native_paste(text)?;
        if bytes.is_empty() {
            return Ok(false);
        }
        if bytes.len() > input_queue::INPUT_LIMIT {
            self.input
                .enqueue_with_limit(bytes, MAX_STREAMED_PASTE_BYTES)?;
        } else {
            self.input.enqueue(bytes)?;
        }
        Ok(true)
    }

    fn admit_input(&self) -> Result<(), String> {
        if self.closed || self.is_exited() || self.write_error.is_some() {
            return Err("input unavailable".into());
        }
        if matches!(self.input_boundary, Some(InputBoundary::Raw { .. })) {
            return Err("finish the pending raw input event before submitting a new one".into());
        }
        Ok(())
    }

    pub fn input_idle(&self) -> bool {
        self.input.is_empty() && self.input_boundary.is_none()
    }

    /// Both the integrated prompt and the kernel foreground group must still
    /// belong to the directly launched shell before inserting recalled text.
    pub fn at_shell_prompt(&self) -> bool {
        self.input_idle()
            && self.terminal.engine.command_prompt_ready()
            && self.child_pid().is_some_and(|pid| {
                self.io.as_ref().and_then(|io| io.actor.foreground_pgid()) == Some(pid)
            })
    }

    /// Revoke an already-started logical event when its view relinquishes authority.
    /// Bytes accepted by the kernel cannot be recalled.
    pub fn retire_partial_input(&mut self) -> bool {
        self.seal_input_boundary(BOUNDARY_RETIRED)
    }

    /// Cancel queued input when the view detaches. A partial write still
    /// retires the writer until close.
    pub fn retire_pending_input(&mut self) {
        self.retire_partial_input();
        self.input.clear();
    }

    fn seal_input_boundary(&mut self, reason: &str) -> bool {
        if self.input_boundary.take().is_none() {
            return false;
        }
        self.input.clear();
        self.fail_writer(reason.into());
        true
    }

    fn expire_input_boundary(&mut self) {
        if self
            .input_boundary
            .is_some_and(|boundary| boundary.started().elapsed() >= INPUT_BOUNDARY_TIMEOUT)
        {
            self.seal_input_boundary(BOUNDARY_STALLED);
        }
    }

    pub fn drive(&mut self) -> Result<usize, String> {
        self.drive_with_sync_capture(false)
    }

    /// Advance terminal output, replies, and admitted input under this owner.
    /// Once a logical event starts, following replies cannot split its bytes.
    pub fn drive_with_sync_capture(&mut self, capture: bool) -> Result<usize, String> {
        self.expire_input_boundary();
        let boundary = self.input_boundary;
        let read = if boundary.is_some() {
            self.poll_holding_replies(capture)
        } else {
            if self.holding_replies {
                self.release_held_replies();
            }
            self.poll_with_sync_capture(capture)
        };
        let read = match read {
            Ok(read) => read,
            Err(error) => {
                self.seal_input_boundary(BOUNDARY_FAILED);
                return Err(error);
            }
        };
        if self.is_exited() || self.write_error.is_some() {
            self.seal_input_boundary(BOUNDARY_FAILED);
            self.input.clear();
            return Ok(read);
        }

        self.expire_input_boundary();
        if self.write_error.is_some() {
            return Ok(read);
        }
        if matches!(boundary, Some(InputBoundary::Raw { .. })) {
            return Ok(read);
        }
        let held = matches!(boundary, Some(InputBoundary::Queued(_)));
        if !self.input.is_empty() {
            let mut pending = std::mem::take(&mut self.input);
            let result = if held && pending.partial_remaining().is_some() {
                pending.flush_partial(|bytes, prefix, remaining| {
                    self.continue_partial_input(bytes, prefix, remaining)
                })
            } else if held {
                Ok(()) // Release the already-ordered replies before another logical event.
            } else {
                pending.flush(|bytes| self.write(bytes))
            };
            self.input = pending;
            if result.is_err() || self.write_error.is_some() {
                self.seal_input_boundary(BOUNDARY_FAILED);
                self.input.clear();
                return Ok(read);
            }
        }

        if held {
            self.expire_input_boundary();
            if self.input_boundary.is_some() && self.input.partial_remaining().is_none() {
                let released = self.release_held_replies();
                if self.write_error.is_some() {
                    self.seal_input_boundary(BOUNDARY_FAILED);
                } else if released {
                    self.input_boundary = None;
                }
            }
        } else if self.ordered_input && self.input.partial_remaining().is_some() {
            self.input_boundary = Some(InputBoundary::Queued(Instant::now()));
        }
        Ok(read)
    }

    pub fn is_exited(&self) -> bool {
        self.exited
            || self.io.as_ref().is_some_and(|io| io.actor.leader_exited())
            || self
                .reap_ticket
                .reaped
                .load(std::sync::atomic::Ordering::Acquire)
    }
    /// Close returns after handoff; this remains true until actual reaping.
    pub fn cleanup_pending(&self) -> bool {
        self.reap_ticket
            .pending
            .load(std::sync::atomic::Ordering::Acquire)
    }
    /// Poll until the actor's exit tail has been applied to the grid.
    pub fn is_drained(&self) -> bool {
        self.is_exited() && self.read_eof
    }
    /// Sticky write failure. Read-side polling remains available for final output.
    pub fn write_error(&self) -> Option<&str> {
        self.write_error.as_deref()
    }
    pub fn dropped_reply_bytes(&self) -> usize {
        self.dropped_reply_bytes
    }
    fn fail_writer(&mut self, message: String) {
        if self.write_error.is_none() {
            self.write_error = Some(message);
        }
        self.dropped_reply_bytes = self.dropped_reply_bytes.saturating_add(self.replies.len());
        self.clear_replies();
        self.read_budget_blocked = false;
        self.holding_replies = false;
        if let Some(io) = &self.io {
            io.actor.close_input();
        }
    }

    /// Offer the next chunk from an exact raw-input transaction. The caller
    /// retains its suffix and validates its attachment, bytes and receipt.
    pub fn write_transaction_chunk(&mut self, bytes: &[u8]) -> Result<usize, String> {
        self.expire_input_boundary();
        if self.closed || self.is_exited() || self.write_error.is_some() {
            return Err("input unavailable".into());
        }
        let start = match self.input_boundary {
            Some(InputBoundary::Raw { start, remaining }) => {
                if remaining != bytes.len() {
                    return Err("raw input suffix length changed".into());
                }
                start
            }
            Some(InputBoundary::Queued(_)) => return Ok(0),
            None => {
                let written = self.write(bytes)?;
                if written > 0 && written < bytes.len() {
                    self.holding_replies = true;
                    self.input_boundary = Some(InputBoundary::Raw {
                        start: Instant::now(),
                        remaining: bytes.len() - written,
                    });
                }
                return Ok(written);
            }
        };
        let written = match self.write_bytes(bytes) {
            Ok(written) => written,
            Err(error) => {
                self.seal_input_boundary(BOUNDARY_FAILED);
                return Err(error);
            }
        };
        let remaining = bytes.len() - written;
        if remaining == 0 {
            self.input_boundary = None;
            self.release_held_replies();
        } else {
            self.input_boundary = Some(InputBoundary::Raw { start, remaining });
        }
        Ok(written)
    }

    pub fn write(&mut self, input: &[u8]) -> Result<usize, String> {
        if self.closed || self.is_exited() {
            return Err("session is closed".into());
        }
        if input.len() > MAX_INPUT {
            return Err("input byte limit".into());
        }
        if self.holding_replies || !self.input.is_empty() {
            return Ok(0);
        }
        self.flush_replies();
        if let Some(error) = &self.write_error {
            return Err(error.clone());
        }
        if !self.replies.is_empty() {
            return Ok(0);
        }
        self.write_bytes(input)
    }

    /// Continue the active logical event while protocol replies wait.
    fn continue_partial_input(
        &mut self,
        suffix: &[u8],
        accepted_prefix: std::num::NonZeroU16,
        remaining: usize,
    ) -> Result<usize, String> {
        if self.closed || self.is_exited() {
            return Err("session is closed".into());
        }
        if !self.holding_replies
            || remaining == 0
            || usize::from(accepted_prefix.get()) + remaining > MAX_STREAMED_PASTE_BYTES
            || suffix.len() != remaining.min(input_queue::WRITE_BATCH)
        {
            return Err("partial input continuation size".into());
        }
        if let Some(error) = &self.write_error {
            return Err(error.clone());
        }
        self.write_bytes(suffix)
    }

    fn write_bytes(&mut self, input: &[u8]) -> Result<usize, String> {
        // Caller owns unaccepted suffix. Interrupted/WouldBlock consume no input.
        match self.io.as_mut().ok_or("writer closed")?.write(input) {
            Ok(n) => {
                self.terminal.engine.observe_prompt_input(&input[..n]);
                Ok(n)
            }
            Err(message) => {
                self.fail_writer(message.clone());
                Err(message)
            }
        }
    }
    fn flush_replies(&mut self) {
        // Finite work even if a backend repeatedly interrupts or accepts one byte.
        for _ in 0..64 {
            if self.replies.is_empty() {
                break;
            }
            let result = match self.io.as_mut() {
                Some(io) => io.write(self.replies.make_contiguous()),
                None => Err("writer closed".into()),
            };
            match result {
                Ok(0) => break,
                Ok(n) => {
                    self.replies.drain(..n);
                    self.reply_charge.release(n);
                }
                Err(error) => {
                    self.fail_writer(error);
                    break;
                }
            }
        }
    }
    fn clear_replies(&mut self) {
        let pending = self.replies.len();
        self.replies.clear();
        self.reply_charge.release(pending);
    }

    fn queue_replies(&mut self, replies: Vec<u8>, reservation: &mut Charge) {
        if self.write_error.is_some() {
            self.dropped_reply_bytes = self.dropped_reply_bytes.saturating_add(replies.len());
        } else if self.replies.len() + replies.len() > READ_BUDGET {
            self.dropped_reply_bytes = self.dropped_reply_bytes.saturating_add(replies.len());
            self.fail_writer("pending reply byte limit".into());
        } else {
            reservation.transfer_to(&mut self.reply_charge, replies.len());
            self.replies.extend(replies);
        }
    }

    fn reserve_read_replies(&mut self) -> Option<Charge> {
        let mut reservation = self.reply_charge.sibling();
        // Reserve the full remaining reply allowance before removing any
        // bytes from the reader. Under pressure the bounded event channel
        // backs up to the PTY instead of parsing and discarding replies.
        self.read_budget_blocked = self.write_error.is_none()
            && reservation
                .try_add(READ_BUDGET - self.replies.len(), true)
                .is_err();
        (!self.read_budget_blocked).then_some(reservation)
    }
    pub fn take_completed_sync_frame(&mut self) -> Option<CellFrame> {
        self.terminal.engine.take_completed_sync_frame()
    }
    pub fn poll(&mut self) -> Result<usize, String> {
        self.poll_with_sync_capture(false)
    }
    /// Advance detached output once the owner has retired any partial input.
    #[doc(hidden)]
    pub fn poll_detached_idle(&mut self) -> Result<bool, String> {
        if !self.input_idle() || self.is_drained() {
            return Ok(false);
        }
        self.drive()?;
        Ok(!self.is_drained())
    }

    /// Opt-in bounded snapshot at first completed sync block. Parsing never stops.
    pub fn poll_with_sync_capture(&mut self, capture: bool) -> Result<usize, String> {
        self.poll_output(capture, !self.holding_replies)
    }

    /// Parse and drain while the current logical input event finishes.
    fn poll_holding_replies(&mut self, capture: bool) -> Result<usize, String> {
        self.holding_replies = self.write_error.is_none();
        self.poll_output(capture, false)
    }

    /// Release deferred replies at the confirmed input event boundary.
    fn release_held_replies(&mut self) -> bool {
        self.flush_replies();
        if self.replies.is_empty() {
            self.holding_replies = false;
            true
        } else {
            false
        }
    }

    fn poll_output(&mut self, capture: bool, flush_replies: bool) -> Result<usize, String> {
        self.terminal.engine.begin_update(capture);
        if self.closed {
            return Err("session is closed".into());
        }
        // A reply failure must never prevent draining the PTY's final output.
        if flush_replies {
            self.flush_replies();
        }
        let mut total = 0;
        for _ in 0..64 {
            if self.read_eof || total >= READ_BUDGET {
                break;
            }
            let Some(mut reply_reservation) = self.reserve_read_replies() else {
                break;
            };
            let mut bytes = [0; 4096];
            let remaining = (READ_BUDGET - total).min(bytes.len());
            match self.io.as_mut().ok_or("reader closed")?.read(
                &mut bytes[..remaining],
                &mut self.exit_code,
                &mut self.exit_tail,
            ) {
                Ok(0) => {
                    self.read_eof = true;
                    self.exited |= self.exit_code.is_some();
                    if self.child.exit_outcome() == Some(ChildExitOutcome::Unavailable) {
                        self.exit_code = None;
                        self.fail_writer("child wait status unavailable".into());
                        return Err("child wait status unavailable".into());
                    }
                    break;
                }
                Ok(n) => {
                    total += n;
                    let replies = self.terminal.apply(&bytes[..n])?;
                    if replies.refused_bytes == 0 {
                        self.queue_replies(replies.bytes, &mut reply_reservation);
                    } else {
                        self.dropped_reply_bytes = self
                            .dropped_reply_bytes
                            .saturating_add(replies.bytes.len())
                            .saturating_add(replies.refused_bytes);
                        self.fail_writer("terminal feed reply budget exceeded".into());
                    }
                    if flush_replies {
                        self.flush_replies();
                    }
                }
                Err(e) if e.raw_os_error() == Some(libc::EIO) => {
                    // Linux PTY master reports EIO after the final slave closes.
                    self.read_eof = true;
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        if self.read_eof && !self.replies.is_empty() {
            self.fail_writer("PTY ended before replies were delivered".into());
        }
        Ok(total)
    }
    pub fn resize(&mut self, rows: usize, cols: usize) -> Result<(), String> {
        if self.closed {
            return Err("session is closed".into());
        }
        let actor = &self.io.as_ref().ok_or("master closed")?.actor;
        self.terminal
            .engine
            .resize_with(rows, cols, || actor.resize(cols, rows))
    }
    /// Drop PTY handles and transfer owned child to the fixed-capacity reaper.
    /// Success means accepted for asynchronous cleanup, not confirmed exit.
    pub fn close(&mut self) -> Result<(), String> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.input.clear();
        self.input_boundary = None;
        self.dropped_reply_bytes = self.dropped_reply_bytes.saturating_add(self.replies.len());
        self.clear_replies();
        self.read_budget_blocked = false;
        if let Some(io) = self.io.take() {
            io.actor.stop();
        }
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue_test_replies(session: &mut Session, bytes: Vec<u8>) {
        let mut reservation = session.reserve_read_replies().unwrap();
        session.queue_replies(bytes, &mut reservation);
    }

    #[test]
    fn process_reply_pressure_defers_the_exact_unread_stream_until_capacity_returns() {
        use crate::process_input_budget::Budget;

        let budget = Arc::new(Budget::new(READ_BUDGET, 0));
        let mut session = Session::spawn(4, 80).unwrap();
        session.reply_charge = budget.empty_charge();
        session.terminal.engine.enable_device_answers();
        session.test_io().reader = Some(Box::new(Available(Cursor::new(b"KEPT_OUTPUT\x1b[5n"))));
        let mut other_owner = budget.empty_charge();
        other_owner.try_add(1, true).unwrap();

        assert_eq!(session.poll().unwrap(), 0);
        assert!(session.needs_write_retry());
        assert!(!session.terminal().visible_text().contains("KEPT_OUTPUT"));
        assert_eq!(session.dropped_reply_bytes(), 0);
        assert_eq!(budget.usage().reserved_bytes, 1);

        drop(other_owner);
        // Keep replies queued so the assertion sees the exact retained bytes.
        session.holding_replies = true;
        assert_eq!(session.poll().unwrap(), b"KEPT_OUTPUT\x1b[5n".len());
        assert!(session.terminal().visible_text().contains("KEPT_OUTPUT"));
        assert_eq!(session.replies, VecDeque::from(*b"\x1b[0n"));
        assert_eq!(session.dropped_reply_bytes(), 0);
        assert_eq!(budget.usage().reserved_bytes, 4);
        session.clear_replies();
        assert_eq!(budget.usage().reserved_bytes, 0);
        session.close().unwrap();
    }

    #[test]
    fn real_pty_keeps_output_and_device_query_while_process_capacity_is_held() {
        use crate::process_input_budget::Budget;

        let argv = [
            "/bin/sh".into(),
            "-c".into(),
            concat!(
                "/usr/bin/stty raw -echo; printf 'BUDGET_BEFORE\\033[5n'; ",
                "reply=$(/usr/bin/dd bs=1 count=4 status=none); ",
                "[ \"$reply\" = \"$(printf '\\033[0n')\" ] || exit 42; ",
                "printf '\\r\\nBUDGET_AFTER'; exit 17"
            )
            .into(),
        ];
        let budget = Arc::new(Budget::new(READ_BUDGET, 0));
        let mut session = Session::spawn_command(4, 80, &argv, Some(Path::new("/")), &[]).unwrap();
        session.terminal.engine.enable_device_answers();
        session.reply_charge = budget.empty_charge();
        let mut other_owner = budget.empty_charge();
        other_owner.try_add(1, true).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while session.io.as_ref().unwrap().actor.events().is_empty() {
            assert!(
                Instant::now() < deadline,
                "PTY never published its first output event"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(session.drive().unwrap(), 0);
        assert!(session.needs_write_retry());
        assert!(!session.terminal().visible_text().contains("BUDGET_BEFORE"));
        assert!(!session.io.as_ref().unwrap().actor.events().is_empty());

        drop(other_owner);
        while !session.is_drained() {
            assert!(
                Instant::now() < deadline,
                "PTY did not recover after releasing the process budget"
            );
            session.drive().unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(session.exit_code(), Some(17));
        let visible = session.terminal().visible_text();
        assert!(visible.contains("BUDGET_BEFORE") && visible.contains("BUDGET_AFTER"));
        assert_eq!(session.dropped_reply_bytes(), 0);
        assert_eq!(budget.usage().reserved_bytes, 0);
        session.close().unwrap();
        await_cleanup(&session);
    }
    use std::io::Write;

    impl Session {
        fn test_io(&mut self) -> &mut SessionIo {
            self.io.as_mut().unwrap()
        }

        fn kernel_size(&self) -> libc::winsize {
            self.io
                .as_ref()
                .unwrap()
                .actor
                .inspect_transport(Transport::size)
        }
    }
    fn await_cleanup(s: &Session) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while s.cleanup_pending() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!s.cleanup_pending(), "owned child did not finish cleanup");
    }

    #[test]
    fn explicit_command_uses_child_path_and_closes_unregistered_descriptors() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        let input = std::fs::File::open("/dev/null").unwrap();
        // SAFETY: input is live. F_DUPFD returns an owned descriptor whose
        // close-on-exec flag is clear so the launcher must isolate it.
        let fd = unsafe { libc::fcntl(input.as_raw_fd(), libc::F_DUPFD, 128) };
        assert!(fd >= 128);
        let ambient = unsafe { OwnedFd::from_raw_fd(fd) };
        let args = [
            "sh".into(),
            "-c".into(),
            concat!(
                "test ! -e \"/proc/self/fd/$AMBIENT_FD\" || exit 78; ",
                "printf 'LAUNCHED %s %s %s\\n' \"$0\" \"$VALUE\" \"$PWD\"; exit 127"
            )
            .into(),
        ];
        let environment = [
            ("PATH".into(), "bin".into()),
            ("VALUE".into(), "first".into()),
            ("VALUE".into(), "last".into()),
            ("AMBIENT_FD".into(), ambient.as_raw_fd().to_string().into()),
        ];
        let mut session =
            Session::spawn_command(6, 72, &args, Some(Path::new("/")), &environment).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.is_drained() {
            assert!(
                Instant::now() < deadline,
                "standalone command did not drain"
            );
            session.drive().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(session.exit_code(), Some(127));
        assert!(session
            .terminal()
            .visible_text()
            .contains("LAUNCHED sh last /"));
        assert!(unsafe { libc::fcntl(ambient.as_raw_fd(), libc::F_GETFD) } >= 0);
        session.close().unwrap();
        await_cleanup(&session);
    }

    #[test]
    fn pinned_command_uses_the_open_directory_and_retires_its_descriptor() {
        use std::os::fd::{AsFd, AsRawFd};
        use std::os::unix::fs::DirBuilderExt;

        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let path =
            std::env::temp_dir().join(format!("jterm-pinned-command-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let scratch = Scratch(path);
        let selected = scratch.0.join("selected");
        std::fs::create_dir(&selected).unwrap();
        std::fs::write(selected.join("receipt"), "PINNED_DIRECTORY\n").unwrap();
        let pinned = std::fs::File::open(&selected).unwrap();
        std::fs::rename(&selected, scratch.0.join("retained")).unwrap();
        std::fs::create_dir(&selected).unwrap();
        let args = [
            "/bin/sh".into(),
            "-c".into(),
            concat!(
                "test ! -e \"/proc/self/fd/$DIRECTORY_FD\" || exit 78; ",
                "IFS= read -r receipt < receipt || exit 79; ",
                "printf '%s:%s\\n' \"$receipt\" \"$VALIDATION_ENV\"; exit 7"
            )
            .into(),
        ];
        let environment = [
            ("DIRECTORY_FD".into(), pinned.as_raw_fd().to_string().into()),
            ("VALIDATION_ENV".into(), "isolated".into()),
        ];
        let mut session = Session::spawn_pinned_command(4, 80, &args, pinned.as_fd(), &environment)
            .expect("launch pinned validation command");
        drop(pinned);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.is_drained() {
            assert!(Instant::now() < deadline, "pinned command did not drain");
            session.drive().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(session.exit_code(), Some(7));
        assert!(session
            .terminal()
            .visible_text()
            .contains("PINNED_DIRECTORY:isolated"));
        session.close().unwrap();
        await_cleanup(&session);
    }

    #[test]
    fn forwarded_command_receives_only_the_requested_descriptor() {
        use std::os::fd::AsRawFd;

        let directory = std::fs::File::open("/tmp").unwrap();
        let unrelated = std::fs::File::open("/").unwrap();
        let child_fd = 9;
        let args = [
            "/bin/sh".into(),
            "-c".into(),
            concat!(
                "test /proc/self/fd/9 -ef /tmp && test \"$PWD\" = / || exit 78; ",
                "test ! \"/proc/self/fd/$UNRELATED_FD\" -ef / || exit 79; ",
                "printf FORWARDED; exit 7"
            )
            .into(),
        ];
        let env = [(
            "UNRELATED_FD".into(),
            unrelated.as_raw_fd().to_string().into(),
        )];
        let mut session = Session::spawn_command_with_forwarded_fd(
            4,
            80,
            &args,
            directory.as_fd(),
            child_fd,
            &env,
        )
        .expect("launch descriptor-forwarding command");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.is_drained() {
            assert!(
                Instant::now() < deadline,
                "forwarding command did not drain"
            );
            session.drive().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(session.exit_code(), Some(7));
        assert!(session.terminal().visible_text().contains("FORWARDED"));
        let flags = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        session.close().unwrap();
        await_cleanup(&session);
    }

    struct ScriptedWriter {
        steps: VecDeque<Result<usize, std::io::ErrorKind>>,
        bytes: Vec<u8>,
    }

    struct Available(Cursor<&'static [u8]>);
    impl Read for Available {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.read(bytes)?;
            if n == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            Ok(n)
        }
    }

    impl Write for ScriptedWriter {
        fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
            match self.steps.pop_front().unwrap_or(Ok(input.len())) {
                Ok(n) => {
                    let n = n.min(input.len());
                    self.bytes.extend(&input[..n]);
                    Ok(n)
                }
                Err(kind) => Err(kind.into()),
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn interrupted_and_partial_write_preserve_suffix() {
        use std::io::ErrorKind::{Interrupted, WouldBlock};
        let mut writer = ScriptedWriter {
            steps: [Err(Interrupted), Ok(2), Err(WouldBlock), Ok(2)].into(),
            bytes: vec![],
        };
        assert_eq!(
            bounded_write(b"abcd", |bytes| writer.write(bytes)).unwrap(),
            2
        );
        assert_eq!(
            bounded_write(b"cd", |bytes| writer.write(bytes)).unwrap(),
            0
        );
        assert_eq!(
            bounded_write(b"cd", |bytes| writer.write(bytes)).unwrap(),
            2
        );
        assert_eq!(writer.bytes, b"abcd");
        writer.steps = [Err(Interrupted); 8].into();
        assert_eq!(bounded_write(b"e", |bytes| writer.write(bytes)).unwrap(), 0);
        writer.steps = [Ok(0)].into();
        assert_eq!(
            bounded_write(b"e", |bytes| writer.write(bytes))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::WriteZero
        );
    }
    #[test]
    fn held_reply_waits_for_a_confirmed_continuation_before_ordinary_input() {
        use std::io::{Cursor, ErrorKind};
        use std::sync::Mutex;

        struct Capture {
            steps: VecDeque<Result<usize, ErrorKind>>,
            wire: Arc<Mutex<Vec<u8>>>,
        }
        impl Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let written = self.steps.pop_front().unwrap_or(Ok(bytes.len()))?;
                self.wire
                    .lock()
                    .unwrap()
                    .extend_from_slice(&bytes[..written]);
                Ok(written)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut session = Session::spawn(4, 80).unwrap();
        session.enable_ordered_input();
        let wire = Arc::new(Mutex::new(Vec::new()));
        session.test_io().writer = Some(Box::new(Capture {
            steps: [Ok(6), Err(ErrorKind::WouldBlock)].into(),
            wire: wire.clone(),
        }));
        session.test_io().reader = Some(Box::new(Available(Cursor::new(b""))));
        session
            .submit_input(b"\x1b[200~OWNED\x1b[201~".to_vec())
            .unwrap();
        session.submit_input(b"FRESH".to_vec()).unwrap();
        assert_eq!(session.drive().unwrap(), 0);
        assert_eq!(&*wire.lock().unwrap(), b"\x1b[200~");
        assert!(!session.input_idle());

        session.test_io().reader = Some(Box::new(Available(Cursor::new(b"\x1b[6n"))));
        assert_eq!(session.drive().unwrap(), 4);
        assert!(!session.is_exited());
        assert_eq!(session.write(b"FRESH").unwrap(), 0);
        assert_eq!(&*wire.lock().unwrap(), b"\x1b[200~");
        session.drive().unwrap();
        assert_eq!(&*wire.lock().unwrap(), b"\x1b[200~OWNED\x1b[201~\x1b[1;1R");
        assert!(!session.input_idle());
        session.drive().unwrap();
        assert_eq!(
            &*wire.lock().unwrap(),
            b"\x1b[200~OWNED\x1b[201~\x1b[1;1RFRESH"
        );
        assert!(session.input_idle());

        wire.lock().unwrap().clear();
        session.test_io().writer = Some(Box::new(Capture {
            steps: [Ok(3), Err(ErrorKind::WouldBlock)].into(),
            wire: wire.clone(),
        }));
        session.test_io().reader = Some(Box::new(Available(Cursor::new(b"\x1b[?2004h"))));
        session.drive().unwrap();
        let large_paste = "background input界\r\n".repeat(2100);
        assert!(session.submit_native_paste(&large_paste).unwrap());
        assert!(session
            .submit_input(b"WOULD EXCEED NORMAL BUDGET".to_vec())
            .is_err());
        session.drive().unwrap();
        assert_eq!(&*wire.lock().unwrap(), b"\x1b[2");
        session.test_io().reader = Some(Box::new(Available(Cursor::new(b"\x1b[6n"))));
        session.drive().unwrap();
        assert_eq!(&*wire.lock().unwrap(), b"\x1b[2");
        let mut expected = format!(
            "\x1b[200~{}\x1b[201~\x1b[1;1R",
            large_paste.replace("\r\n", "\r")
        )
        .into_bytes();
        for _ in 0..MAX_STREAMED_PASTE_BYTES.div_ceil(input_queue::WRITE_BATCH) + 2 {
            if session.input_idle() {
                break;
            }
            session.drive().unwrap();
        }
        assert!(session.input_idle());
        assert_eq!(*wire.lock().unwrap(), expected);
        session.submit_input(b"AFTER".to_vec()).unwrap();
        session.drive().unwrap();
        expected.extend(b"AFTER");
        assert_eq!(*wire.lock().unwrap(), expected);
        session.close().unwrap();
        await_cleanup(&session);
    }

    #[test]
    fn exact_raw_write_suffix_holds_replies_and_excludes_other_input() {
        struct FirstPrefix(Vec<u8>);
        impl Write for FirstPrefix {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.0.is_empty() {
                    assert_eq!(bytes, b"raw-line\n");
                    self.0.push(bytes[0]);
                    return Ok(1);
                }
                self.0.extend_from_slice(bytes);
                assert!(b"raw-line\n\x1b[1;1R".starts_with(&self.0));
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut session = Session::spawn(4, 80).unwrap();
        session.test_io().writer = Some(Box::new(FirstPrefix(Vec::new())));
        assert_eq!(session.write_transaction_chunk(b"raw-line\n").unwrap(), 1);
        assert!(session.write_transaction_chunk(b"wrong size").is_err());
        assert!(session.submit_input(b"ordinary input".to_vec()).is_err());
        assert_eq!(session.write(b"legacy input").unwrap(), 0);
        let replies = session.terminal.apply(b"\x1b[6n").unwrap().bytes;
        queue_test_replies(&mut session, replies);
        assert_eq!(session.replies.len(), 6);
        assert_eq!(session.write_transaction_chunk(b"aw-line\n").unwrap(), 8);
        assert!(session.replies.is_empty());
        assert!(session.input_idle());
        session.close().unwrap();
        await_cleanup(&session);
    }

    #[test]
    fn bundled_bash_native_transport_paints_grid_and_reaps_real_shell() {
        let script = br#"
            identity_fd=$ANVIL_SHELL_INTEGRATION_FD
            IFS= read -r -u "$identity_fd" identity
            exec {identity_fd}<&-
            if [[ $identity =~ ^[a-f0-9]{32}$ && -z ${ANVIL_SHELL_INTEGRATION_TOKEN+x} ]]; then
                printf 'NATIVE_BASH_READY\n'
            fi
            PS1='native> '
        "#;
        let environment = [
            ("TERM".into(), "dumb".into()),
            ("HISTFILE".into(), "/dev/null".into()),
            ("ANVIL_SHELL_INTEGRATION_FD".into(), "0".into()),
            ("ANVIL_SHELL_INTEGRATION_TOKEN".into(), "bogus".into()),
        ];
        let mut session = Session::spawn_anvil_bash(
            12,
            80,
            Path::new("/bin/bash"),
            script,
            Path::new("/"),
            &environment,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session
            .terminal()
            .visible_text()
            .contains("NATIVE_BASH_READY")
        {
            assert!(
                Instant::now() < deadline,
                "Bash did not read its private descriptor"
            );
            session.drive().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        session.resize(14, 94).unwrap();
        let size = session.kernel_size();
        assert_eq!((size.ws_row, size.ws_col), (14, 94));
        session
            .submit_input(b"printf '\\nNATIVE_GRID_RESULT\\n'; exit 7\r".to_vec())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.is_drained() {
            assert!(
                Instant::now() < deadline,
                "Bash did not deliver final output and exit"
            );
            session.drive().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(session.exit_code(), Some(7));
        assert!(session
            .terminal()
            .visible_text()
            .lines()
            .any(|line| line.trim_end() == "NATIVE_GRID_RESULT"));
        session.close().unwrap();
        await_cleanup(&session);
    }

    #[test]
    fn retiring_a_raw_prefix_does_not_send_eof_or_stop_detached_output() {
        let argv = [
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from("stty -echo; (sleep .2; printf '\\nDETACHED_OUTPUT\\n\\033[6n') & printf 'PTY_READY\\n'; read -r input; printf 'INPUT_WOKE:%s\\n' \"$input\"; wait"),
        ];
        let mut session = Session::spawn_command(4, 80, &argv, None, &[]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.terminal().visible_text().contains("PTY_READY") {
            assert!(Instant::now() < deadline);
            session.drive().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        session.test_io().write_limit = Some(2);
        assert_eq!(session.write_transaction_chunk(b"partial\n").unwrap(), 2);
        assert!(session.retire_partial_input());
        while session.dropped_reply_bytes() == 0 {
            assert!(Instant::now() < deadline);
            assert!(session.poll_detached_idle().unwrap());
            std::thread::sleep(Duration::from_millis(1));
        }
        let output = session.terminal().visible_text();
        assert!(output.contains("DETACHED_OUTPUT"));
        assert!(!output.contains("INPUT_WOKE"));
        assert!(session.write_transaction_chunk(b"rtial\n").is_err());
        session.close().unwrap();
        await_cleanup(&session);
    }

    #[test]
    fn configured_session_owns_the_command_input_exit_and_tail() {
        let argv = [
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from("read -r input; printf 'RECEIPT:%s:%s:%s\\n' \"$PWD\" \"$JTERM_TEST_VALUE\" \"$input\"; exit 7"),
        ];
        let env = [("JTERM_TEST_VALUE".into(), "prepared-environment".into())];
        let mut session = Session::spawn_command(4, 80, &argv, Some(Path::new("/")), &env)
            .expect("start configured PTY");
        assert!(session.child_pid().is_some());
        assert_eq!(session.exit_code(), None);
        session.submit_input(b"queued-input\n".to_vec()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.is_drained() && Instant::now() < deadline {
            session.drive().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(session.is_drained());
        assert!(session.input_idle());
        assert_eq!(session.child_pid(), None);
        assert_eq!(session.exit_code(), Some(7));
        assert!(session
            .terminal()
            .visible_text()
            .contains("RECEIPT:/:prepared-environment:queued-input"));
        assert!(session.submit_input(b"late".to_vec()).is_err());
        session.close().unwrap();
        await_cleanup(&session);
    }

    #[test]
    fn broken_reply_writer_does_not_hide_output_tail() {
        let mut session = Session::spawn(4, 80).unwrap();
        session.test_io().writer = Some(Box::new(ScriptedWriter {
            steps: [Ok(1), Err(std::io::ErrorKind::BrokenPipe)].into(),
            bytes: vec![],
        }));
        queue_test_replies(&mut session, b"reply".to_vec());
        session.test_io().reader =
            Some(Box::new(std::io::Cursor::new(b"TAIL_AFTER_REPLY_FAILURE")));
        assert_eq!(session.poll().unwrap(), 24);
        assert!(session
            .terminal()
            .visible_text()
            .contains("TAIL_AFTER_REPLY_FAILURE"));
        assert!(session.write_error().is_some());
        assert_eq!(session.dropped_reply_bytes(), 4);
        assert!(session.replies.is_empty());
        assert!(session.write(b"x").is_err());
        assert_eq!(session.poll().unwrap(), 0);
        session.close().unwrap();
    }
    #[test]
    fn exit_is_not_drained_until_budgeted_tail_reaches_eof() {
        let mut session = Session::spawn(4, 80).unwrap();
        let mut bytes = vec![b'x'; READ_BUDGET];
        bytes.extend_from_slice(b"\r\nFINAL_TAIL");
        session.test_io().reader = Some(Box::new(std::io::Cursor::new(bytes)));
        session.exited = true; // Deterministic ordering: exit observed before final reads.
        assert!(!session.is_drained());
        assert_eq!(session.poll().unwrap(), READ_BUDGET);
        assert!(!session.is_drained());
        assert_eq!(session.poll().unwrap(), 12);
        assert!(session.is_drained());
        assert!(session.terminal().visible_text().contains("FINAL_TAIL"));
        session.close().unwrap();
    }
    #[test]
    fn reply_overflow_fails_only_writer_and_counts_discard() {
        let mut session = Session::spawn(4, 80).unwrap();
        queue_test_replies(&mut session, vec![b'a'; READ_BUDGET]);
        queue_test_replies(&mut session, vec![b'b'; 1]);
        assert_eq!(session.write_error(), Some("pending reply byte limit"));
        assert_eq!(session.dropped_reply_bytes(), READ_BUDGET + 1);
        queue_test_replies(&mut session, vec![b'c'; 2]);
        assert_eq!(session.dropped_reply_bytes(), READ_BUDGET + 3);
        assert!(session.replies.is_empty());
        session.close().unwrap();
    }

    #[test]
    fn one_read_can_exhaust_reply_budget_without_losing_its_output_tail() {
        let mut session = Session::spawn(4, 80).unwrap();
        session.enable_window_updates();
        let title = "🦀".repeat(200);
        let report_len = b"\x1b]l\x1b\\".len() + title.len();
        let queries = READ_BUDGET / report_len + 1;
        let mut bytes = format!("\x1b]2;{title}\x07").into_bytes();
        bytes.extend(b"\x1b[21t".repeat(queries));
        bytes.extend(b"TAIL_IN_THE_SAME_READ");
        let read_len = bytes.len();
        assert!(read_len < 4096);
        session.test_io().reader = Some(Box::new(std::io::Cursor::new(bytes)));

        assert_eq!(session.poll().unwrap(), read_len);
        assert_eq!(
            session.write_error(),
            Some("terminal feed reply budget exceeded")
        );
        assert_eq!(session.dropped_reply_bytes(), report_len * queries);
        assert!(session
            .terminal()
            .visible_text()
            .contains("TAIL_IN_THE_SAME_READ"));
        assert_eq!(session.poll().unwrap(), 0);
        session.close().unwrap();
    }

    #[test]
    fn real_pty_status_query_replies_without_publishing_a_new_frame() {
        let argv = [
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(
                "stty raw -echo || exit 30; printf READY; IFS= read -r start || exit 31; \
                 printf '\\033[5n'; answer=$(dd bs=1 count=4 2>/dev/null); \
                 [ \"$answer\" = \"$(printf '\\033[0n')\" ] || exit 32; \
                 IFS= read -r finish || exit 33; printf DONE",
            ),
        ];
        let mut session = Session::spawn_command(4, 80, &argv, None, &[]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.terminal().visible_text().contains("READY") {
            assert!(
                Instant::now() < deadline,
                "child did not reach its input gate"
            );
            session.drive().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        let before = session.terminal().frame().unwrap().stamp();
        session.submit_input(b"start\n".to_vec()).unwrap();
        let mut read = 0;
        while read < b"\x1b[5n".len() {
            assert!(Instant::now() < deadline, "child did not send its query");
            read += session.drive().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(session.terminal().frame().unwrap().stamp(), before);

        session.submit_input(b"finish\n".to_vec()).unwrap();
        while !session.is_drained() {
            assert!(Instant::now() < deadline, "child did not finish");
            session.drive().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            session.exit_code(),
            Some(0),
            "child validates the reply bytes"
        );
        assert!(session.terminal().visible_text().contains("READYDONE"));
        assert!(session.terminal().frame().unwrap().stamp().revision > before.revision);
        session.close().unwrap();
    }

    #[test]
    fn interrupted_reads_are_bounded_and_eio_marks_eof() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct InterruptedReader(Arc<AtomicUsize>);
        impl Read for InterruptedReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(std::io::ErrorKind::Interrupted.into())
            }
        }
        struct EndedReader;
        impl Read for EndedReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(libc::EIO))
            }
        }
        let mut session = Session::spawn(4, 80).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        session.test_io().reader = Some(Box::new(InterruptedReader(calls.clone())));
        assert_eq!(session.poll().unwrap(), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 64);
        assert!(!session.read_eof);
        session.test_io().reader = Some(Box::new(EndedReader));
        assert_eq!(session.poll().unwrap(), 0);
        assert!(session.read_eof);
        session.close().unwrap();
    }
    #[test]
    fn fixed_shell_real_pty_archives_scrolled_output() {
        let mut s = Session::spawn(2, 80).unwrap();
        let command = b"printf 'HISTORY_A\\nHISTORY_B\\nHISTORY_C\\nHISTORY_D\\n'; exit\n";
        let mut sent = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !s.is_drained() && std::time::Instant::now() < deadline {
            if sent < command.len() && !s.is_exited() {
                sent += s.write(&command[sent..]).unwrap();
            }
            s.poll().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(s.is_drained());
        let page = s.terminal().history_page(0, 128).unwrap();
        assert!(
            page.text.lines().any(|line| line == "HISTORY_B"),
            "{}",
            page.text
        );
        assert!(page.row_count > 0);
        assert_eq!(
            s.terminal().history_page(1024, 128).unwrap().offset,
            page.total_rows
        );
        assert!(s.terminal().history_page(usize::MAX, 128).is_err());
        s.close().unwrap();
    }
    #[test]
    fn short_reads_cannot_overshoot_poll_byte_budget() {
        struct ShortFirst {
            first: bool,
            bytes: std::io::Cursor<Vec<u8>>,
        }
        impl Read for ShortFirst {
            fn read(&mut self, target: &mut [u8]) -> std::io::Result<usize> {
                if self.first {
                    self.first = false;
                    self.bytes.read(&mut target[..1])
                } else {
                    self.bytes.read(target)
                }
            }
        }
        let mut s = Session::spawn(2, 80).unwrap();
        s.test_io().reader = Some(Box::new(ShortFirst {
            first: true,
            bytes: std::io::Cursor::new(vec![b'x'; READ_BUDGET + 1]),
        }));
        assert_eq!(s.poll().unwrap(), READ_BUDGET);
        assert!(!s.read_eof);
        assert_eq!(s.poll().unwrap(), 1);
        assert!(s.read_eof);
        s.close().unwrap();
    }
    #[test]
    fn real_pty_large_exit_tail_is_drained_without_budget_overrun() {
        for _ in 0..3 {
            let mut s = Session::spawn(3, 80).unwrap();
            let command = b"printf '%070000d\\nEXIT_TAIL_VERIFIED\\n' 0; exit\n";
            let mut sent = 0;
            let mut received = 0;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !s.is_drained() && std::time::Instant::now() < deadline {
                if !s.is_exited() && sent < command.len() {
                    sent += s.write(&command[sent..]).unwrap();
                }
                let n = s.poll().unwrap();
                assert!(n <= READ_BUDGET);
                received += n;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(s.is_drained());
            assert!(received > 70000);
            assert!(s
                .terminal()
                .visible_text()
                .lines()
                .any(|line| line.trim_end() == "EXIT_TAIL_VERIFIED"));
            assert!(s.write(b"x").is_err());
            s.close().unwrap();
            s.close().unwrap();
            assert!(s.poll().is_err());
        }
    }

    #[test]
    fn actor_observes_leader_exit_before_the_pump_applies_final_output() {
        let argv = [
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from("printf '%04100d\\nACTOR_BUFFERED_TAIL\\n' 0; exit 17"),
        ];
        let mut session = Session::spawn_command(4, 80, &argv, None, &[]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.is_exited() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            session.is_exited(),
            "the actor must run without a grid pump"
        );
        assert!(!session.is_drained());
        assert_eq!(session.exit_code(), None);
        assert!(session.write_transaction_chunk(b"late input").is_err());
        while !session.is_drained() && Instant::now() < deadline {
            assert!(session.drive().unwrap() <= READ_BUDGET);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(session.is_drained());
        assert_eq!(session.exit_code(), Some(17));
        assert!(session
            .terminal()
            .visible_text()
            .contains("ACTOR_BUFFERED_TAIL"));
    }
    #[test]
    fn real_pty_repeated_live_close_is_idempotent() {
        for _ in 0..8 {
            let mut s = Session::spawn(2, 80).unwrap();
            s.close().unwrap();
            s.close().unwrap();
            await_cleanup(&s);
            assert!(s.is_exited());
            assert!(s.io.is_none());
            assert!(s.resize(3, 90).is_err());
            assert!(s.write(b"x").is_err());
        }
    }
    #[test]
    fn real_pty_preserves_the_shared_signal_exit_code() {
        let argv = [
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from("printf SIGNAL_TAIL; kill -TERM \"$$\""),
        ];
        let mut session = Session::spawn_command(2, 80, &argv, None, &[]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.is_drained() && Instant::now() < deadline {
            session.drive().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(session.is_drained());
        assert_eq!(session.exit_code(), Some(128 + libc::SIGTERM));
        assert!(session.terminal().visible_text().contains("SIGNAL_TAIL"));
        session.close().unwrap();
        await_cleanup(&session);
    }
    #[test]
    fn real_pty_close_reaps_shell_and_drains_a_distinct_background_group() {
        use crate::process_stat::process_stat;

        let argv = [
            OsString::from("/bin/sh"),
            OsString::from("-ic"),
            OsString::from("trap '' HUP TERM; set -m; (while :; do sleep 30; done) & printf 'PIDFD_READY=%s\\n' \"$!\"; while :; do sleep 30; done"),
        ];
        let mut s = Session::spawn_command(4, 100, &argv, None, &[]).unwrap();
        let leader = s.child_pid().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let descendant = loop {
            s.poll().unwrap();
            let output = s.terminal().visible_text();
            if let Some(pid) = output
                .lines()
                .filter_map(|line| line.trim().strip_prefix("PIDFD_READY="))
                .find_map(|value| value.parse::<i32>().ok())
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "PTY job did not report its PID: {output}"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        let background = process_stat(descendant).unwrap();
        assert_eq!(background.session, leader);
        assert_ne!(background.process_group, leader);
        assert!(background.is_live());
        assert!(!s.is_exited());
        s.close().unwrap();
        await_cleanup(&s);
        assert!(s.is_exited());
        assert!(process_stat(leader).is_none());
        assert!(process_stat(descendant).is_none_or(|stat| !stat.is_live()));
        s.close().unwrap();
    }
    // Feed the production parser rather than calling UnicodeWidthChar directly:
    // FF9E/FF9F changed from zero to one cell in unicode-width 0.2.3. These
    // assertions cover host geometry, not WebView glyph shaping or IME input.
    #[test]
    fn real_pty_chinese_wide_overwrite_and_wrap_frame() {
        let mut s = Session::spawn(4, 8).unwrap();
        // Fixed printf emits UTF-8 in separate writes; explicit cursor addressing
        // removes shell prompt and echo from the area under assertion.
        let command = b"printf '\\033[2J\\033[H'; printf '\\347'; printf '\\225\\214'; printf '\\033[1;2HA\\033[2;8H\\344\\270\\255'; exit\n";
        let mut sent = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !s.is_drained() && std::time::Instant::now() < deadline {
            if sent < command.len() && !s.is_exited() {
                sent += s.write(&command[sent..]).unwrap();
            }
            s.poll().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(s.is_drained());
        let frame = s.terminal().frame().unwrap();
        assert_eq!(frame.rows()[0].cells[0].character, ' ');
        assert_eq!(frame.rows()[0].cells[1].character, 'A');
        assert_eq!(frame.rows()[2].cells[0].character, '中');
        assert!(frame.rows()[2].cells[0].flags.wide());
        assert!(frame.rows()[2].cells[1].flags.wide_continuation());
        s.close().unwrap();
        await_cleanup(&s);
    }
    #[test]
    fn real_pty_resize_limits_match_kernel_and_reject_without_mutation() {
        let mut s = Session::spawn(4, 8).unwrap();
        for (rows, cols) in [(1, 1), (128, 256), (3, 9)] {
            s.resize(rows, cols).unwrap();
            let kernel = s.kernel_size();
            assert_eq!(
                (kernel.ws_row as usize, kernel.ws_col as usize),
                (rows, cols)
            );
            let frame = s.terminal().frame().unwrap();
            assert_eq!((frame.rows().len(), frame.cols()), (rows, cols));
        }
        // Real slave output drives DECSCUSR, visibility and clamped positioning.
        let command = b"printf '\\033[6 q\\033[?25l\\033[999;999H'; exit\n";
        let mut sent = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !s.is_drained() && std::time::Instant::now() < deadline {
            if sent < command.len() && !s.is_exited() {
                sent += s.write(&command[sent..]).unwrap();
            }
            s.poll().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(s.is_drained());
        let before = s.terminal().frame().unwrap();
        assert_eq!(before.cursor().shape, jterm_engine::CursorShape::Beam);
        assert!(!before.cursor().visible);
        assert_eq!((before.cursor().row, before.cursor().col), (2, 8));
        for (rows, cols) in [(0, 1), (1, 0), (129, 1), (1, 257)] {
            assert!(s.resize(rows, cols).is_err());
        }
        let kernel = s.kernel_size();
        assert_eq!((kernel.ws_row, kernel.ws_col), (3, 9));
        assert_eq!(s.terminal().frame().unwrap().stamp(), before.stamp());
        s.close().unwrap();
        assert!(s.resize(4, 8).is_err());
        assert_eq!(s.terminal().frame().unwrap().stamp(), before.stamp());
        await_cleanup(&s);
    }
    #[test]
    fn fixed_shell_real_pty_roundtrip_resize_and_close() {
        let mut s = Session::spawn(8, 80).unwrap();
        s.resize(10, 90).unwrap();
        let command = b"printf 'PTY_OK_314159\\n'; exit\n";
        let mut sent = 0;
        let start = std::time::Instant::now();
        while !s.is_drained() && start.elapsed() < std::time::Duration::from_secs(5) {
            if !s.is_exited() && sent < command.len() {
                sent += s.write(&command[sent..]).unwrap();
            }
            s.poll().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Drain output after the exit was observed.
        s.poll().unwrap();
        assert!(s.is_exited());
        assert!(s.is_drained());
        let text = s.terminal().visible_text();
        assert!(
            text.lines()
                .any(|l| l.trim_end().ends_with("PTY_OK_314159")),
            "{text}"
        );
        s.close().unwrap();
        s.close().unwrap();
        assert!(s.write(b"x").is_err());
    }
}
