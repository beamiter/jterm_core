//! Shared worker for one-shot `--print` / stream-json provider CLIs.
//!
//! Claude Code and Kimi Code both run one prompt per process and report it as
//! newline-delimited JSON on stdout. Their parsers differ; everything else
//! (process ownership, cancellation, the lifecycle event contract, bounded
//! text and diagnostics) lives here so the two adapters cannot drift apart.
//!
//! Lifecycle contract with [`crate::agent_task::TaskManager`]:
//! - exactly one `SessionStarted` per session, followed by one `TurnStarted`;
//!   a provider session id is attached only when the CLI reports it before
//!   any other output (Claude's `system/init`), otherwise it is recorded in the
//!   view snapshot alone (Kimi prints its resume hint after the turn);
//! - terminal events (`TurnCompleted`, `SessionEnded`) are emitted only after
//!   the CLI's process group has been killed, its root reaped, and the group
//!   probed empty, matching the Codex driver.
//!
//! Containment is weaker than Codex's cgroup scope: the CLI leads a private
//! process group, and `containment_verified_empty` means that *group* was
//! observed empty after SIGKILL. A descendant that deliberately left the group
//! (`setsid`/`setpgid`) is outside what this MVP can see or verify.

use crate::agent_task::driver::{
    agent_event_channel, AgentCancellation, AgentCommand, AgentDriverError, AgentEventReceiveError,
    AgentEventReceiver, AgentEventSender, AgentEventSink, AgentStartRequest,
};
use crate::agent_task::drivers::codex_app_server::{
    append_visible_bounded, visible_bounded, CodexAppServerExitCause, CodexAppServerExitReport,
    CodexAppServerPhase, CodexAppServerProcessExit, CodexAppServerViewSnapshot,
    CODEX_APP_SERVER_AGENT_TEXT_MAX_BYTES,
};
use crate::agent_task::event::MAX_AGENT_EVENT_DETAIL_BYTES;
use crate::agent_task::{
    AgentEvent, AgentEventKind, AgentProvider, AgentSessionOutcome, AgentTurnId, ProviderSessionId,
};
use crate::supervised::SupervisedChild;
use crossbeam_channel::{bounded, never, select, Receiver, Sender};
use parking_lot::Mutex;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const COMMAND_CAPACITY: usize = 8;
/// Retained stderr diagnostics (the *last* bytes the CLI wrote).
const STDERR_TAIL_MAX_BYTES: usize = 64 * 1024;
/// One stdout JSON record. Claude's final `result` repeats the answer text,
/// so this is generous; anything longer is a protocol failure, not an OOM.
pub(super) const STDOUT_LINE_MAX_BYTES: usize = 8 * 1024 * 1024;
const STDOUT_LINE_QUEUE: usize = 64;
/// Worker wake-up interval; bounds Stop/Drop latency while a tool runs.
const WORKER_POLL: Duration = Duration::from_millis(25);
/// How long a provider that reports its session id first may take to do so
/// before `SessionStarted` is emitted without one.
const SESSION_ID_WAIT: Duration = Duration::from_secs(5);
/// After the root exits, a descendant may still hold stdout open.
const ROOT_EXIT_STDOUT_DRAIN: Duration = Duration::from_secs(2);
/// After stdout EOF, the root should exit on its own.
const EXIT_AFTER_EOF_WAIT: Duration = Duration::from_secs(5);
/// SIGTERM → SIGKILL grace for the whole process group.
const TERMINATE_GRACE: Duration = Duration::from_millis(750);
/// SIGKILLed members are reaped by init/subreaper asynchronously.
const CONTAINMENT_VERIFY_WAIT: Duration = Duration::from_secs(2);
const READER_JOIN_WAIT: Duration = Duration::from_millis(500);
const PROCESS_POLL: Duration = Duration::from_millis(10);
/// Failure details shown to the user (status line / exit report).
pub(super) const FAILURE_DETAIL_MAX_BYTES: usize = MAX_AGENT_EVENT_DETAIL_BYTES;
/// Stderr lines folded into a failure detail when nothing better exists.
const STDERR_DETAIL_LINES: usize = 3;

/// Provider-neutral meaning of one stdout record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PrintSignal {
    /// The provider's own session identity.
    SessionId(String),
    /// Assistant-visible text.
    Text(String),
    /// Non-fatal provider status, such as a transient API retry.
    Status(String),
    /// The provider's terminal verdict for the turn.
    Finished {
        success: bool,
        text: Option<String>,
        detail: Option<String>,
    },
    /// A fatal provider error reported in-band.
    Fatal(String),
    Ignored,
}

/// Static per-provider wiring for [`PrintStreamDriver`].
pub(super) struct PrintProviderSpec {
    pub provider: AgentProvider,
    pub label: &'static str,
    pub thread_suffix: &'static str,
    /// Environment variables removed before spawning the CLI.
    pub env_remove: &'static [&'static str],
    /// Wait (bounded) for [`PrintSignal::SessionId`] before `SessionStarted`.
    pub session_id_first: bool,
    pub argv: fn(&[String], &str) -> Result<Vec<String>, String>,
    pub parse: fn(&str) -> PrintSignal,
}

/// Shared native driver for one-shot print/stream-json CLIs.
pub(super) struct PrintStreamDriver {
    spec: &'static PrintProviderSpec,
    launch_argv: Vec<String>,
    worktree_path: PathBuf,
    command_sender: Sender<AgentCommand>,
    command_receiver: Option<Receiver<AgentCommand>>,
    event_sender: Option<AgentEventSender>,
    event_receiver: AgentEventReceiver,
    cancellation: AgentCancellation,
    view: Arc<Mutex<CodexAppServerViewSnapshot>>,
    exit_report: Arc<Mutex<Option<CodexAppServerExitReport>>>,
    worker: Option<JoinHandle<()>>,
    started: bool,
}

impl PrintStreamDriver {
    pub fn new(
        spec: &'static PrintProviderSpec,
        launch_argv: Vec<String>,
        worktree_path: PathBuf,
    ) -> Self {
        let (event_sender, event_receiver) = agent_event_channel();
        let (command_sender, command_receiver) = bounded(COMMAND_CAPACITY);
        Self {
            spec,
            launch_argv,
            worktree_path,
            command_sender,
            command_receiver: Some(command_receiver),
            event_sender: Some(event_sender),
            event_receiver,
            cancellation: AgentCancellation::new(),
            view: Arc::new(Mutex::new(CodexAppServerViewSnapshot {
                phase: CodexAppServerPhase::Created,
                ..CodexAppServerViewSnapshot::default()
            })),
            exit_report: Arc::new(Mutex::new(None)),
            worker: None,
            started: false,
        }
    }

    pub fn view_snapshot(&self) -> CodexAppServerViewSnapshot {
        self.view.lock().clone()
    }

    pub fn phase(&self) -> CodexAppServerPhase {
        self.view.lock().phase
    }

    pub fn take_exit_report(&self) -> Option<CodexAppServerExitReport> {
        self.exit_report.lock().take()
    }

    pub fn worker_is_finished(&self) -> bool {
        self.worker.as_ref().is_some_and(JoinHandle::is_finished)
    }

    pub fn join_finished_worker(&mut self) -> Result<bool, AgentDriverError> {
        if !self.worker_is_finished() {
            return Ok(false);
        }
        let Some(worker) = self.worker.take() else {
            return Ok(false);
        };
        let label = self.spec.label;
        worker.join().map_err(|_| {
            let mut report = self.exit_report.lock();
            if report.is_none() {
                *report = Some(CodexAppServerExitReport {
                    outcome: AgentSessionOutcome::Failed,
                    cause: CodexAppServerExitCause::WorkerPanicked,
                    detail: Some(format!("{label} stream-json worker panicked")),
                    process: CodexAppServerProcessExit::default(),
                    critical_event_delivery_failed: true,
                    stderr_tail: String::new(),
                });
            }
            AgentDriverError::Provider(format!("{label} stream-json worker panicked"))
        })?;
        Ok(true)
    }

    pub fn start(&mut self, request: AgentStartRequest) -> Result<(), AgentDriverError> {
        let label = self.spec.label;
        if self.started {
            return Err(AgentDriverError::AlreadyStarted);
        }
        request.validate_for_provider(self.spec.provider)?;
        if request.resume_from.is_some() {
            return Err(AgentDriverError::Provider(format!(
                "{label} stream-json resume is not enabled for native task sessions"
            )));
        }
        if request.worktree_path != self.worktree_path {
            return Err(AgentDriverError::InvalidWorktree);
        }
        let prompt = request.initial_prompt.ok_or_else(|| {
            AgentDriverError::Provider(format!("{label} MVP requires an initial prompt"))
        })?;
        let argv = (self.spec.argv)(&self.launch_argv, &prompt.text)
            .map_err(AgentDriverError::Provider)?;
        let event_sender = self.event_sender.take().ok_or(AgentDriverError::Closed)?;
        let command_receiver = self
            .command_receiver
            .take()
            .ok_or(AgentDriverError::Closed)?;
        let turn_id = prompt.turn_id;
        {
            let mut snapshot = self.view.lock();
            snapshot.phase = CodexAppServerPhase::Spawning;
            snapshot.displayed_turn_id = Some(turn_id);
            snapshot.displayed_turn_ordinal = Some(1);
        }
        let worker = PrintWorker {
            spec: self.spec,
            sink: AgentEventSink::new(request.stream, event_sender),
            view: Arc::clone(&self.view),
            turn_id,
            session_started: false,
            fatal: None,
            cause: None,
            critical_failure: None,
        };
        let exit_report = Arc::clone(&self.exit_report);
        let cancellation = self.cancellation.clone();
        let worktree_path = self.worktree_path.clone();
        let handle = thread::Builder::new()
            .name(format!(
                "{}-{}",
                crate::agent_task::app_slug(),
                self.spec.thread_suffix
            ))
            .spawn(move || {
                let report = worker.run(&argv, &worktree_path, command_receiver, &cancellation);
                *exit_report.lock() = Some(report);
            })
            .map_err(|error| {
                AgentDriverError::Provider(format!("could not start {label} worker: {error}"))
            })?;
        self.worker = Some(handle);
        self.started = true;
        Ok(())
    }

    pub fn send(&mut self, command: AgentCommand) -> Result<(), AgentDriverError> {
        if !self.started {
            return Err(AgentDriverError::NotStarted);
        }
        command.validate()?;
        if self.cancellation.is_cancelled() {
            return Err(AgentDriverError::Closed);
        }
        match &command {
            AgentCommand::FinishSession => {}
            AgentCommand::Prompt(_)
            | AgentCommand::Steer { .. }
            | AgentCommand::DecideApproval { .. } => {
                return Err(AgentDriverError::Provider(format!(
                    "{} MVP is one-shot print mode; use Terminal fallback for follow-up turns",
                    self.spec.label
                )));
            }
        }
        self.command_sender
            .try_send(command)
            .map_err(|_| AgentDriverError::Backpressure {
                queued_messages: COMMAND_CAPACITY,
                message_capacity: COMMAND_CAPACITY,
            })
    }

    /// Only flips the shared token; the worker observes it within
    /// [`WORKER_POLL`] even while the CLI is silent inside a long tool call.
    pub fn cancel(&mut self) {
        self.cancellation.cancel();
    }

    pub fn try_next_event(&mut self) -> Result<Option<AgentEvent>, AgentDriverError> {
        if !self.started {
            return Err(AgentDriverError::NotStarted);
        }
        match self.event_receiver.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(AgentEventReceiveError::Empty) => Ok(None),
            Err(AgentEventReceiveError::Closed) => Err(AgentDriverError::Closed),
        }
    }
}

impl Drop for PrintStreamDriver {
    fn drop(&mut self) {
        self.cancellation.cancel();
        // Never detach a live provider: the worker notices cancellation within
        // one poll interval, kills the whole process group, and reaps it.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum StdoutMessage {
    Line(String),
    Overlong,
    Failed(String),
    Eof,
}

enum LineRead {
    Line,
    Overlong,
    Eof,
}

/// Read one `\n`-terminated record of at most `max` bytes. An overlong record
/// is consumed through its newline without being retained.
fn read_bounded_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    max: usize,
) -> io::Result<LineRead> {
    line.clear();
    let mut overlong = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Ok(if overlong {
                LineRead::Overlong
            } else if line.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Line
            });
        }
        let (chunk, done) = match available.iter().position(|byte| *byte == b'\n') {
            Some(index) => (&available[..index], Some(index + 1)),
            None => (available, None),
        };
        if !overlong {
            if line.len() + chunk.len() > max {
                overlong = true;
                line.clear();
            } else {
                line.extend_from_slice(chunk);
            }
        }
        let consumed = done.unwrap_or(available.len());
        reader.consume(consumed);
        if done.is_some() {
            return Ok(if overlong {
                LineRead::Overlong
            } else {
                LineRead::Line
            });
        }
    }
}

fn spawn_stdout_reader(
    stdout: impl Read + Send + 'static,
    label: &'static str,
) -> io::Result<(Receiver<StdoutMessage>, JoinHandle<()>)> {
    let (sender, receiver) = bounded(STDOUT_LINE_QUEUE);
    let handle = thread::Builder::new()
        .name(format!("{}-agent-stdout", crate::agent_task::app_slug()))
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                let message = match read_bounded_line(&mut reader, &mut line, STDOUT_LINE_MAX_BYTES)
                {
                    Ok(LineRead::Line) => {
                        StdoutMessage::Line(String::from_utf8_lossy(&line).into_owned())
                    }
                    Ok(LineRead::Overlong) => StdoutMessage::Overlong,
                    Ok(LineRead::Eof) => StdoutMessage::Eof,
                    Err(error) => {
                        StdoutMessage::Failed(format!("{label} stdout read failed: {error}"))
                    }
                };
                let last = matches!(message, StdoutMessage::Eof | StdoutMessage::Failed(_));
                // The worker drops the receiver when it stops listening.
                if sender.send(message).is_err() || last {
                    return;
                }
            }
        })?;
    Ok((receiver, handle))
}

fn spawn_stderr_reader(
    stderr: impl Read + Send + 'static,
    tail: Arc<Mutex<Vec<u8>>>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("{}-agent-stderr", crate::agent_task::app_slug()))
        .spawn(move || {
            let mut stderr = stderr;
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        append_tail(&mut tail.lock(), &chunk[..read], STDERR_TAIL_MAX_BYTES)
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
}

/// Keep the newest `max` bytes.
fn append_tail(tail: &mut Vec<u8>, bytes: &[u8], max: usize) {
    if bytes.len() >= max {
        tail.clear();
        tail.extend_from_slice(&bytes[bytes.len() - max..]);
        return;
    }
    let overflow = (tail.len() + bytes.len()).saturating_sub(max);
    if overflow > 0 {
        tail.drain(..overflow);
    }
    tail.extend_from_slice(bytes);
}

/// Last non-empty stderr lines as one bounded, display-safe detail.
pub(super) fn stderr_failure_detail(stderr_tail: &str) -> Option<String> {
    let lines: Vec<&str> = stderr_tail
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    let start = lines.len().saturating_sub(STDERR_DETAIL_LINES);
    let joined = lines[start..].join(" | ");
    Some(bounded_detail(&joined))
}

/// Display-safe, bounded provider text for statuses and failure details.
pub(super) fn bounded_detail(text: &str) -> String {
    let (mut bounded, truncated) = visible_bounded(text.trim(), FAILURE_DETAIL_MAX_BYTES - 3);
    if truncated {
        bounded.push('…');
    }
    bounded
}

fn wait_root_exit(
    child: &mut SupervisedChild,
    timeout: Duration,
    cancel: Option<&AgentCancellation>,
) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.root_has_exited()? {
            return Ok(true);
        }
        if Instant::now() >= deadline || cancel.is_some_and(AgentCancellation::is_cancelled) {
            return Ok(false);
        }
        thread::sleep(PROCESS_POLL);
    }
}

/// True once no process remains in `process_group` (signal-0 probe only).
#[cfg(unix)]
fn process_group_is_empty(process_group: i32) -> bool {
    if process_group <= 1 {
        return false;
    }
    // SAFETY: signal 0 performs only existence/permission checks.
    let result = unsafe { libc::kill(-process_group, 0) };
    result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(unix)]
fn wait_process_group_empty(process_group: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if process_group_is_empty(process_group) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(PROCESS_POLL);
    }
}

fn join_bounded(handle: JoinHandle<()>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            // A descendant that escaped the process group can keep a pipe
            // open indefinitely; the reader exits on its own at EOF.
            return;
        }
        thread::sleep(PROCESS_POLL);
    }
    let _ = handle.join();
}

struct PrintWorker {
    spec: &'static PrintProviderSpec,
    sink: AgentEventSink,
    view: Arc<Mutex<CodexAppServerViewSnapshot>>,
    turn_id: AgentTurnId,
    session_started: bool,
    fatal: Option<String>,
    cause: Option<CodexAppServerExitCause>,
    critical_failure: Option<String>,
}

impl PrintWorker {
    fn fail(&mut self, cause: CodexAppServerExitCause, detail: String) {
        if self.fatal.is_none() {
            self.fatal = Some(bounded_detail(&detail));
            self.cause = Some(cause);
        }
    }

    fn emit_critical(&mut self, kind: AgentEventKind, detail: Option<String>) {
        if self.critical_failure.is_some() {
            return;
        }
        if let Err(error) = self.sink.try_emit(kind, detail) {
            self.critical_failure = Some(format!(
                "{} lifecycle event could not be delivered: {error}",
                self.spec.label
            ));
        }
    }

    fn emit_update(&self, kind: AgentEventKind, detail: Option<String>) {
        match self.sink.try_emit(kind, detail) {
            Ok(_) => {}
            Err(error) if error.is_backpressure() => {
                let mut view = self.view.lock();
                view.dropped_updates = view.dropped_updates.saturating_add(1);
            }
            // Closed: the runtime stopped listening; terminal handling below
            // still records the outcome in the exit report.
            Err(_) => {}
        }
    }

    /// Emit the session's single `SessionStarted` (and its turn) exactly once.
    fn ensure_started(&mut self, provider_session_id: Option<&str>) {
        if self.session_started {
            return;
        }
        self.session_started = true;
        let provider_session_id =
            provider_session_id.and_then(|id| ProviderSessionId::new(self.spec.provider, id).ok());
        self.emit_critical(
            AgentEventKind::SessionStarted {
                provider_session_id,
                resumed: false,
            },
            None,
        );
        self.emit_critical(
            AgentEventKind::TurnStarted {
                turn_id: self.turn_id,
            },
            None,
        );
    }

    fn append_text(&self, text: &str) {
        let mut view = self.view.lock();
        let mut agent_text = std::mem::take(&mut view.agent_text);
        let mut truncated = view.agent_text_truncated;
        append_visible_bounded(
            &mut agent_text,
            &mut truncated,
            text,
            CODEX_APP_SERVER_AGENT_TEXT_MAX_BYTES,
        );
        view.agent_text = agent_text;
        view.agent_text_truncated = truncated;
    }

    /// Assistant text is retained (bounded) in the view snapshot; the event
    /// itself carries only a bounded preview, like Codex's `emit_delta`.
    /// `AgentEvent` caps details at [`MAX_AGENT_EVENT_DETAIL_BYTES`], so a
    /// whole message never becomes one oversized or silently-dropped event.
    fn emit_text(&self, text: &str) {
        self.append_text(text);
        let (preview, _) = visible_bounded(text, MAX_AGENT_EVENT_DETAIL_BYTES);
        self.emit_update(
            AgentEventKind::TextDelta,
            (!preview.is_empty()).then_some(preview),
        );
    }

    fn handle(&mut self, signal: PrintSignal) {
        match signal {
            PrintSignal::SessionId(session_id) => {
                let (visible, _) = visible_bounded(&session_id, FAILURE_DETAIL_MAX_BYTES);
                self.view.lock().provider_thread_id = Some(visible);
                self.ensure_started(Some(&session_id));
            }
            PrintSignal::Text(text) => {
                self.ensure_started(None);
                self.emit_text(&text);
            }
            PrintSignal::Status(status) => {
                self.ensure_started(None);
                self.emit_update(
                    AgentEventKind::Error { fatal: false },
                    Some(bounded_detail(&status)),
                );
            }
            PrintSignal::Finished {
                success,
                text,
                detail,
            } => {
                self.ensure_started(None);
                if let Some(text) = text.filter(|text| !text.is_empty()) {
                    if self.view.lock().agent_text.is_empty() {
                        self.emit_text(&text);
                    }
                }
                if !success {
                    let detail = detail.unwrap_or_else(|| {
                        format!("{} print session reported failure", self.spec.label)
                    });
                    self.fail(CodexAppServerExitCause::ProviderFailed, detail);
                }
            }
            PrintSignal::Fatal(detail) => {
                self.ensure_started(None);
                self.fail(CodexAppServerExitCause::ProviderFailed, detail);
            }
            PrintSignal::Ignored => {}
        }
    }

    fn spawn(&self, argv: &[String], worktree_path: &Path) -> Result<SupervisedChild, String> {
        let label = self.spec.label;
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| format!("{label} launch argv is empty"))?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(worktree_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in self.spec.env_remove {
            command.env_remove(name);
        }
        // Private process group: Stop/Drop signal the CLI *and* every tool
        // subprocess it started, not just the leader.
        SupervisedChild::spawn(&mut command)
            .map_err(|error| format!("failed to spawn {label}: {error}"))
    }

    fn run(
        mut self,
        argv: &[String],
        worktree_path: &Path,
        command_receiver: Receiver<AgentCommand>,
        cancellation: &AgentCancellation,
    ) -> CodexAppServerExitReport {
        let label = self.spec.label;
        let mut child = match self.spawn(argv, worktree_path) {
            Ok(child) => child,
            Err(detail) => {
                return self.finish(
                    CodexAppServerProcessExit::default(),
                    AgentSessionOutcome::Failed,
                    CodexAppServerExitCause::SpawnFailed,
                    Some(bounded_detail(&detail)),
                    String::new(),
                );
            }
        };
        #[cfg(unix)]
        let process_group = child.process_group_id();
        self.view.lock().phase = CodexAppServerPhase::Running;
        if !self.spec.session_id_first {
            self.ensure_started(None);
        }

        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_reader = child
            .take_stderr()
            .and_then(|stderr| spawn_stderr_reader(stderr, Arc::clone(&stderr_tail)).ok());
        let stdout = child
            .take_stdout()
            .ok_or_else(|| format!("{label} stdout pipe was unavailable"))
            .and_then(|stdout| {
                spawn_stdout_reader(stdout, label)
                    .map_err(|error| format!("could not start {label} stdout reader: {error}"))
            });
        let (stdout_receiver, stdout_reader) = match stdout {
            Ok((receiver, handle)) => (Some(receiver), Some(handle)),
            Err(detail) => {
                self.fail(CodexAppServerExitCause::IoFailed, detail);
                (None, None)
            }
        };

        let started_at = Instant::now();
        let mut cancelled = false;
        let mut force_stop = self.fatal.is_some();
        let commands = command_receiver;
        let mut commands_open = true;
        let closed_commands = never::<AgentCommand>();
        let mut root_exited_at: Option<Instant> = None;
        if let Some(lines) = stdout_receiver.as_ref() {
            loop {
                if cancellation.is_cancelled() {
                    cancelled = true;
                    break;
                }
                if self.critical_failure.is_some() {
                    force_stop = true;
                    break;
                }
                let command_source = if commands_open {
                    &commands
                } else {
                    &closed_commands
                };
                let mut stdout_done = false;
                select! {
                    recv(lines) -> message => match message {
                        Ok(StdoutMessage::Line(line)) => self.handle((self.spec.parse)(&line)),
                        Ok(StdoutMessage::Overlong) => {
                            self.fail(
                                CodexAppServerExitCause::ProtocolFailed,
                                format!("{label} stdout record exceeded {STDOUT_LINE_MAX_BYTES} bytes"),
                            );
                            force_stop = true;
                            stdout_done = true;
                        }
                        Ok(StdoutMessage::Failed(detail)) => {
                            self.fail(CodexAppServerExitCause::IoFailed, detail);
                            stdout_done = true;
                        }
                        Ok(StdoutMessage::Eof) | Err(_) => stdout_done = true,
                    },
                    recv(command_source) -> command => match command {
                        Ok(AgentCommand::FinishSession) => cancellation.cancel(),
                        Ok(_) => {}
                        Err(_) => commands_open = false,
                    },
                    default(WORKER_POLL) => {}
                }
                if stdout_done {
                    break;
                }
                if !self.session_started && started_at.elapsed() >= SESSION_ID_WAIT {
                    self.ensure_started(None);
                }
                if root_exited_at.is_none() && child.root_has_exited().unwrap_or(true) {
                    root_exited_at = Some(Instant::now());
                }
                if root_exited_at.is_some_and(|at| at.elapsed() >= ROOT_EXIT_STDOUT_DRAIN) {
                    // The CLI is gone but a descendant still holds stdout.
                    break;
                }
            }
        }
        // Drain whatever commands are still queued; nothing more is accepted.
        while commands.try_recv().is_ok() {}
        drop(commands);

        // Stop the process group. A cancelled, failed-to-deliver, or protocol-
        // broken session is terminated; a finished one gets a bounded chance to
        // exit on its own first. Either way the whole group is SIGKILLed before
        // the root is reaped, so no tool subprocess outlives the session.
        let mut exited_on_its_own = false;
        if !cancelled && !force_stop {
            match wait_root_exit(&mut child, EXIT_AFTER_EOF_WAIT, Some(cancellation)) {
                Ok(true) => exited_on_its_own = true,
                Ok(false) if cancellation.is_cancelled() => cancelled = true,
                Ok(false) => self.fail(
                    CodexAppServerExitCause::ProviderFailed,
                    format!("{label} did not exit after closing its output"),
                ),
                Err(error) => self.fail(
                    CodexAppServerExitCause::IoFailed,
                    format!("cannot observe {label} process: {error}"),
                ),
            }
        }
        #[cfg(unix)]
        if !exited_on_its_own && child.terminate_group().is_ok() {
            let _ = wait_root_exit(&mut child, TERMINATE_GRACE, None);
        }
        let status = child.reap_after_group_kill();
        #[cfg(unix)]
        let containment_verified_empty =
            status.is_ok() && wait_process_group_empty(process_group, CONTAINMENT_VERIFY_WAIT);
        #[cfg(not(unix))]
        let containment_verified_empty = false;
        drop(stdout_receiver);
        if let Some(reader) = stdout_reader {
            join_bounded(reader, READER_JOIN_WAIT);
        }
        if let Some(reader) = stderr_reader {
            join_bounded(reader, READER_JOIN_WAIT);
        }
        let stderr_tail = String::from_utf8_lossy(&stderr_tail.lock()).into_owned();

        let process = match &status {
            Ok(status) => process_exit(status, containment_verified_empty),
            Err(_) => CodexAppServerProcessExit {
                spawned: true,
                provider_released: true,
                reaped: false,
                containment_verified_empty: false,
                success: false,
                code: None,
                signal: None,
            },
        };
        if let Err(error) = &status {
            self.fail(
                CodexAppServerExitCause::IoFailed,
                format!("{label} wait failed: {error}"),
            );
        } else if !containment_verified_empty {
            self.fail(
                CodexAppServerExitCause::IoFailed,
                format!("{label} process group could not be verified empty after SIGKILL"),
            );
        }

        if let Some(detail) = self.critical_failure.clone() {
            return self.finish(
                process,
                AgentSessionOutcome::Failed,
                CodexAppServerExitCause::EventDeliveryFailed,
                Some(bounded_detail(&detail)),
                stderr_tail,
            );
        }
        if cancelled {
            return self.finish(
                process,
                AgentSessionOutcome::Cancelled,
                CodexAppServerExitCause::Cancelled,
                None,
                stderr_tail,
            );
        }
        if self.fatal.is_none() && !process.success {
            let detail = stderr_failure_detail(&stderr_tail).unwrap_or_else(|| {
                match (process.code, process.signal) {
                    (Some(code), _) => format!("{label} exited with status {code}"),
                    (None, Some(signal)) => format!("{label} was terminated by signal {signal}"),
                    (None, None) => format!("{label} exited unsuccessfully"),
                }
            });
            self.fail(CodexAppServerExitCause::ProviderFailed, detail);
        }
        match self.fatal.clone() {
            Some(detail) => {
                let cause = self
                    .cause
                    .unwrap_or(CodexAppServerExitCause::ProviderFailed);
                self.finish(
                    process,
                    AgentSessionOutcome::Failed,
                    cause,
                    Some(detail),
                    stderr_tail,
                )
            }
            None => self.finish(
                process,
                AgentSessionOutcome::Clean,
                CodexAppServerExitCause::Clean,
                None,
                stderr_tail,
            ),
        }
    }

    /// Publish the terminal events (only after the process is stopped) and
    /// build the exit report.
    fn finish(
        mut self,
        process: CodexAppServerProcessExit,
        mut outcome: AgentSessionOutcome,
        mut cause: CodexAppServerExitCause,
        mut detail: Option<String>,
        stderr_tail: String,
    ) -> CodexAppServerExitReport {
        if outcome == AgentSessionOutcome::Clean {
            self.ensure_started(None);
            self.emit_critical(
                AgentEventKind::TurnCompleted {
                    turn_id: self.turn_id,
                },
                None,
            );
        }
        let mut critical_event_delivery_failed = false;
        if let Some(failure) = self.critical_failure.take() {
            critical_event_delivery_failed = true;
            outcome = AgentSessionOutcome::Failed;
            cause = CodexAppServerExitCause::EventDeliveryFailed;
            detail = Some(bounded_detail(&failure));
        }
        if let Err(error) = self
            .sink
            .try_emit(AgentEventKind::SessionEnded { outcome }, detail.clone())
        {
            critical_event_delivery_failed = true;
            outcome = AgentSessionOutcome::Failed;
            cause = CodexAppServerExitCause::EventDeliveryFailed;
            detail = Some(bounded_detail(&format!(
                "{} lifecycle event could not be delivered: {error}",
                self.spec.label
            )));
        }
        {
            let mut view = self.view.lock();
            view.phase = match outcome {
                AgentSessionOutcome::Failed => CodexAppServerPhase::Failed,
                AgentSessionOutcome::Clean | AgentSessionOutcome::Cancelled => {
                    CodexAppServerPhase::Ended
                }
            };
            if outcome == AgentSessionOutcome::Clean {
                view.completed_turns = 1;
            }
            if let Some(detail) = &detail {
                view.last_error = Some(detail.clone());
            }
        }
        self.sink.close();
        CodexAppServerExitReport {
            outcome,
            cause,
            detail,
            process,
            critical_event_delivery_failed,
            stderr_tail,
        }
    }
}

fn process_exit(
    status: &ExitStatus,
    containment_verified_empty: bool,
) -> CodexAppServerProcessExit {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal = None;
    CodexAppServerProcessExit {
        spawned: true,
        provider_released: true,
        reaped: true,
        containment_verified_empty,
        success: status.success(),
        code: status.code(),
        signal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_line_reader_splits_and_rejects_overlong_records() {
        let input = b"short\n0123456789\nlast".to_vec();
        let mut reader = BufReader::with_capacity(4, input.as_slice());
        let mut line = Vec::new();
        assert!(matches!(
            read_bounded_line(&mut reader, &mut line, 8).unwrap(),
            LineRead::Line
        ));
        assert_eq!(line, b"short");
        assert!(matches!(
            read_bounded_line(&mut reader, &mut line, 8).unwrap(),
            LineRead::Overlong
        ));
        assert!(matches!(
            read_bounded_line(&mut reader, &mut line, 8).unwrap(),
            LineRead::Line
        ));
        assert_eq!(line, b"last");
        assert!(matches!(
            read_bounded_line(&mut reader, &mut line, 8).unwrap(),
            LineRead::Eof
        ));
    }

    #[test]
    fn stderr_tail_keeps_newest_bytes_and_detail_uses_last_lines() {
        let mut tail = Vec::new();
        append_tail(&mut tail, b"0123456789", 8);
        assert_eq!(tail, b"23456789");
        append_tail(&mut tail, b"ab", 8);
        assert_eq!(tail, b"456789ab");

        assert_eq!(stderr_failure_detail("\n  \n"), None);
        let detail =
            stderr_failure_detail("noise\nfirst\n\nError: \u{1b}[31mauth failed\u{1b}[0m\nbye\n")
                .unwrap();
        assert!(detail.starts_with("first | Error:"), "{detail}");
        assert!(detail.ends_with("| bye"), "{detail}");
        assert!(!detail.contains('\u{1b}'));
        let long = stderr_failure_detail(&"x".repeat(10 * FAILURE_DETAIL_MAX_BYTES)).unwrap();
        assert!(long.len() <= FAILURE_DETAIL_MAX_BYTES);
    }
}

/// Driver-to-reducer harness: run a fake provider CLI (a shell script that
/// replays recorded stdout/stderr) through a real driver worker and apply
/// every event to a real [`crate::agent_task::TaskManager`].
#[cfg(all(test, unix))]
pub(super) mod test_support {
    use crate::agent_task::driver::{
        AgentDriver, AgentDriverError, AgentPrompt, AgentStartRequest,
    };
    use crate::agent_task::{
        AgentEvent, AgentEventStream, AgentProvider, NewTask, TaskId, TaskManager,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "jterm-core-print-{label}-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(std::fs::canonicalize(&path).unwrap())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Write `stdout` verbatim to a data file and a `/bin/sh` fake CLI that
    /// prints it, runs `after` (shell), and exits. Extra argv is ignored.
    pub fn fake_cli(dir: &Path, stdout: &str, after: &str) -> PathBuf {
        let data = dir.join("stdout.jsonl");
        std::fs::write(&data, stdout).unwrap();
        let script = dir.join("fake-cli");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncat '{}'\n{after}\n", data.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// Run the script through `/bin/sh` rather than exec'ing it: a file this
    /// test just wrote may still be open for writing in a sibling test's
    /// forked child, which makes a direct exec fail with ETXTBSY.
    pub fn fake_cli_argv(script: &Path) -> Vec<String> {
        vec!["/bin/sh".to_string(), script.display().to_string()]
    }

    pub fn native_task(
        provider: AgentProvider,
        worktree: &Path,
    ) -> (TaskManager, TaskId, AgentEventStream) {
        let mut manager = TaskManager::new();
        let task_id = manager
            .create(NewTask {
                title: "Fix cargo test".to_string(),
                provider,
                repo_root: PathBuf::from("/nonexistent-repo-root"),
                worktree_path: worktree.to_path_buf(),
                branch: "app/task-print".to_string(),
                base_commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
                source_context: None,
            })
            .unwrap();
        let stream = manager.start_agent_event_stream(task_id).unwrap();
        (manager, task_id, stream)
    }

    pub fn start_request(
        provider: AgentProvider,
        stream: AgentEventStream,
        worktree: &Path,
    ) -> AgentStartRequest {
        AgentStartRequest {
            provider,
            stream,
            worktree_path: worktree.to_path_buf(),
            source_context: None,
            initial_prompt: Some(AgentPrompt::new("fix the failing build")),
            resume_from: None,
        }
    }

    /// Drain until the driver closes its queue, applying each event to the
    /// reducer. Every event must be accepted.
    pub fn drive_to_close(
        driver: &mut dyn AgentDriver,
        manager: &mut TaskManager,
        timeout: Duration,
    ) -> Vec<AgentEvent> {
        let deadline = Instant::now() + timeout;
        let mut events = Vec::new();
        loop {
            match driver.try_next_event() {
                Ok(Some(event)) => {
                    if manager.has_active_agent_event_stream(event.stream().task_id()) {
                        manager
                            .apply_agent_event(event.clone())
                            .unwrap_or_else(|error| {
                                panic!("reducer rejected {:?}: {error}", event.kind())
                            });
                    }
                    events.push(event);
                }
                Ok(None) => {
                    assert!(Instant::now() < deadline, "driver did not finish in time");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(AgentDriverError::Closed) => return events,
                Err(error) => panic!("driver transport failed: {error}"),
            }
        }
    }

    pub fn wait_until_not_live(pid: i32, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while crate::process::process_stat(pid).is_some_and(|stat| stat.is_live()) {
            assert!(Instant::now() < deadline, "process {pid} survived cleanup");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn read_pid(path: &Path, timeout: Duration) -> i32 {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
            {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "fake CLI never wrote its pid file"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
