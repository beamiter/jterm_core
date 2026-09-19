//! Minimal Claude Code adapter using `--print` stream-json.
//!
//! This is deliberately weaker than the Codex app-server path: no private home,
//! no cgroup containment, and one print-mode turn per session. Failures should
//! fall back to the opaque PTY launcher.

use crate::agent_task::driver::{
    agent_event_channel, AgentCancellation, AgentCommand, AgentDriver, AgentDriverError,
    AgentEventReceiveError, AgentEventReceiver, AgentEventSender, AgentEventSink, AgentPrompt,
    AgentStartRequest,
};
use crate::agent_task::drivers::codex_app_server::{
    CodexAppServerExitCause, CodexAppServerExitReport, CodexAppServerPhase,
    CodexAppServerProcessExit, CodexAppServerViewSnapshot,
};
use crate::agent_task::{
    AgentEvent, AgentEventKind, AgentProvider, AgentSessionOutcome, AgentTurnId, ProviderSessionId,
};
use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const COMMAND_CAPACITY: usize = 8;
const AGENT_TEXT_MAX_BYTES: usize = 256 * 1024;
const STDERR_TAIL_MAX_BYTES: usize = 64 * 1024;
const IO_POLL: Duration = Duration::from_millis(20);
const TERMINATE_GRACE: Duration = Duration::from_millis(500);

/// One normalized signal derived from a Claude Code stream-json line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaudeStreamSignal {
    SessionInit {
        session_id: Option<String>,
    },
    TextDelta(String),
    Result {
        success: bool,
        result_text: Option<String>,
    },
    Error(String),
    Ignored,
}

/// Parse one complete Claude Code stream-json line.
pub fn parse_stream_json_line(line: &str) -> ClaudeStreamSignal {
    let line = line.trim();
    if line.is_empty() {
        return ClaudeStreamSignal::Ignored;
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return ClaudeStreamSignal::Ignored;
    };
    let Some(event_type) = value.get("type").and_then(Value::as_str) else {
        return ClaudeStreamSignal::Ignored;
    };
    match event_type {
        "system" => {
            let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or("");
            if subtype == "init" {
                let session_id = value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                ClaudeStreamSignal::SessionInit { session_id }
            } else if let Some(error) = value.get("error").and_then(Value::as_str) {
                ClaudeStreamSignal::Error(error.to_owned())
            } else {
                ClaudeStreamSignal::Ignored
            }
        }
        "assistant" => extract_assistant_text(&value)
            .map(ClaudeStreamSignal::TextDelta)
            .unwrap_or(ClaudeStreamSignal::Ignored),
        "stream_event" | "content_block_delta" => extract_text_delta(&value)
            .map(ClaudeStreamSignal::TextDelta)
            .unwrap_or(ClaudeStreamSignal::Ignored),
        "result" => {
            let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or("");
            let success = subtype != "error"
                && value
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .map(|is_error| !is_error)
                    .unwrap_or(true);
            let result_text = value
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| extract_assistant_text(&value));
            if success {
                ClaudeStreamSignal::Result {
                    success: true,
                    result_text,
                }
            } else {
                let detail = value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(result_text)
                    .unwrap_or_else(|| "Claude print session failed".into());
                ClaudeStreamSignal::Error(detail)
            }
        }
        "error" => {
            let detail = value
                .get("error")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Claude stream error")
                .to_owned();
            ClaudeStreamSignal::Error(detail)
        }
        _ => ClaudeStreamSignal::Ignored,
    }
}

fn extract_assistant_text(value: &Value) -> Option<String> {
    let message = value.get("message").unwrap_or(value);
    let content = message.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let Value::Array(blocks) = content else {
        return None;
    };
    let mut out = String::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            out.push_str(text);
        }
    }
    (!out.is_empty()).then_some(out)
}

fn extract_text_delta(value: &Value) -> Option<String> {
    let delta = value.get("delta").or_else(|| {
        value
            .get("event")
            .and_then(|event| event.get("delta"))
    })?;
    if delta.get("type").and_then(Value::as_str) == Some("text_delta")
        || delta.get("type").and_then(Value::as_str).is_none()
    {
        return delta
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
    None
}

/// Build the print-mode argv for one Claude Code native turn.
pub fn claude_print_argv(executable_argv: &[String], prompt: &str) -> Result<Vec<String>, String> {
    if executable_argv.is_empty() {
        return Err("Claude launch argv is empty".into());
    }
    if prompt.trim().is_empty() {
        return Err("Claude print prompt is empty".into());
    }
    let mut argv = executable_argv.to_vec();
    argv.extend([
        "-p".to_string(),
        "--verbose".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--dangerously-skip-permissions".to_string(),
        prompt.to_string(),
    ]);
    Ok(argv)
}

/// Native Claude Code driver (print / stream-json MVP).
pub struct ClaudeStreamJsonDriver {
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

impl ClaudeStreamJsonDriver {
    pub fn new(launch_argv: Vec<String>, worktree_path: PathBuf) -> Self {
        let (event_sender, event_receiver) = agent_event_channel();
        let (command_sender, command_receiver) = bounded(COMMAND_CAPACITY);
        Self {
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
        worker.join().map_err(|_| {
            let mut report = self.exit_report.lock();
            if report.is_none() {
                *report = Some(CodexAppServerExitReport {
                    outcome: AgentSessionOutcome::Failed,
                    cause: CodexAppServerExitCause::WorkerPanicked,
                    detail: Some("Claude stream-json worker panicked".into()),
                    process: CodexAppServerProcessExit::default(),
                    critical_event_delivery_failed: true,
                    stderr_tail: String::new(),
                });
            }
            AgentDriverError::Provider("Claude stream-json worker panicked".into())
        })?;
        Ok(true)
    }
}

impl AgentDriver for ClaudeStreamJsonDriver {
    fn provider(&self) -> AgentProvider {
        AgentProvider::Claude
    }

    fn start(&mut self, request: AgentStartRequest) -> Result<(), AgentDriverError> {
        if self.started {
            return Err(AgentDriverError::AlreadyStarted);
        }
        request.validate_for_provider(AgentProvider::Claude)?;
        if request.resume_from.is_some() {
            return Err(AgentDriverError::Provider(
                "Claude stream-json resume is not enabled for native task sessions".into(),
            ));
        }
        if request.worktree_path != self.worktree_path {
            return Err(AgentDriverError::InvalidWorktree);
        }
        let prompt = request
            .initial_prompt
            .ok_or_else(|| AgentDriverError::Provider("Claude MVP requires an initial prompt".into()))?;
        let argv = claude_print_argv(&self.launch_argv, &prompt.text).map_err(AgentDriverError::Provider)?;
        let event_sender = self.event_sender.take().ok_or(AgentDriverError::Closed)?;
        let command_receiver = self
            .command_receiver
            .take()
            .ok_or(AgentDriverError::Closed)?;
        let sink = AgentEventSink::new(request.stream, event_sender);
        let view = Arc::clone(&self.view);
        let exit_report = Arc::clone(&self.exit_report);
        let cancellation = self.cancellation.clone();
        let worktree_path = self.worktree_path.clone();
        let turn_id = prompt.turn_id;

        {
            let mut snapshot = view.lock();
            snapshot.phase = CodexAppServerPhase::Spawning;
            snapshot.displayed_turn_id = Some(turn_id);
            snapshot.displayed_turn_ordinal = Some(1);
        }

        let worker = thread::Builder::new()
            .name(format!(
                "{}-claude-stream-json",
                crate::agent_task::app_slug()
            ))
            .spawn(move || {
                run_claude_worker(
                    argv,
                    worktree_path,
                    prompt,
                    turn_id,
                    sink,
                    command_receiver,
                    view,
                    exit_report,
                    cancellation,
                );
            })
            .map_err(|error| {
                AgentDriverError::Provider(format!("could not start Claude worker: {error}"))
            })?;
        self.worker = Some(worker);
        self.started = true;
        Ok(())
    }

    fn send(&mut self, command: AgentCommand) -> Result<(), AgentDriverError> {
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
                return Err(AgentDriverError::Provider(
                    "Claude MVP is one-shot print mode; use Terminal fallback for follow-up turns"
                        .into(),
                ));
            }
        }
        self.command_sender
            .try_send(command)
            .map_err(|_| AgentDriverError::Backpressure {
                queued_messages: COMMAND_CAPACITY,
                message_capacity: COMMAND_CAPACITY,
            })
    }

    fn cancel(&mut self) {
        self.cancellation.cancel();
    }

    fn try_next_event(&mut self) -> Result<Option<AgentEvent>, AgentDriverError> {
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

fn run_claude_worker(
    argv: Vec<String>,
    worktree_path: PathBuf,
    prompt: AgentPrompt,
    turn_id: AgentTurnId,
    sink: AgentEventSink,
    command_receiver: Receiver<AgentCommand>,
    view: Arc<Mutex<CodexAppServerViewSnapshot>>,
    exit_report: Arc<Mutex<Option<CodexAppServerExitReport>>>,
    cancellation: AgentCancellation,
) {
    let _ = prompt;
    let mut child = match spawn_claude(&argv, &worktree_path) {
        Ok(child) => child,
        Err(detail) => {
            publish_failure(
                &sink,
                &view,
                &exit_report,
                CodexAppServerExitCause::SpawnFailed,
                detail,
                CodexAppServerProcessExit::default(),
            );
            return;
        }
    };

    {
        let mut snapshot = view.lock();
        snapshot.phase = CodexAppServerPhase::Running;
    }
    let _ = sink.try_emit(
        AgentEventKind::SessionStarted {
            provider_session_id: None,
            resumed: false,
        },
        None,
    );
    let _ = sink.try_emit(AgentEventKind::TurnStarted { turn_id }, None);

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    let stderr_handle = stderr.map(|stderr| {
        let stderr_tail = Arc::clone(&stderr_tail);
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => append_bounded(&mut stderr_tail.lock(), &chunk[..n], STDERR_TAIL_MAX_BYTES),
                    Err(_) => break,
                }
            }
        })
    });

    let mut saw_result = false;
    let mut fatal_error: Option<String> = None;
    if let Some(stdout) = stdout {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            if cancellation.is_cancelled() {
                break;
            }
            while let Ok(command) = command_receiver.try_recv() {
                if matches!(command, AgentCommand::FinishSession) {
                    cancellation.cancel();
                }
            }
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => match parse_stream_json_line(&line) {
                    ClaudeStreamSignal::SessionInit { session_id } => {
                        if let Some(session_id) = session_id {
                            let mut snapshot = view.lock();
                            snapshot.provider_thread_id = Some(session_id.clone());
                            if let Ok(provider_session) =
                                ProviderSessionId::new(AgentProvider::Claude, &session_id)
                            {
                                let _ = sink.try_emit(
                                    AgentEventKind::SessionStarted {
                                        provider_session_id: Some(provider_session),
                                        resumed: false,
                                    },
                                    None,
                                );
                            }
                        }
                    }
                    ClaudeStreamSignal::TextDelta(text) => {
                        append_agent_text(&mut view.lock(), &text);
                        let _ = sink.try_emit(AgentEventKind::TextDelta, Some(text));
                    }
                    ClaudeStreamSignal::Result {
                        success,
                        result_text,
                    } => {
                        saw_result = true;
                        if let Some(text) = result_text {
                            if view.lock().agent_text.is_empty() {
                                append_agent_text(&mut view.lock(), &text);
                                let _ = sink.try_emit(AgentEventKind::TextDelta, Some(text));
                            }
                        }
                        if !success {
                            fatal_error =
                                Some("Claude print session reported failure".into());
                        }
                    }
                    ClaudeStreamSignal::Error(detail) => {
                        fatal_error = Some(detail.clone());
                        let _ = sink.try_emit(
                            AgentEventKind::Error { fatal: true },
                            Some(detail),
                        );
                    }
                    ClaudeStreamSignal::Ignored => {}
                },
                Err(error) => {
                    fatal_error = Some(format!("Claude stdout read failed: {error}"));
                    break;
                }
            }
            // Keep the UI responsive while Claude streams.
            thread::sleep(IO_POLL);
        }
    }

    if cancellation.is_cancelled() {
        terminate_child(&mut child);
    }
    let status = child.wait();
    if let Some(handle) = stderr_handle {
        let _ = handle.join();
    }
    let process = match status {
        Ok(status) => CodexAppServerProcessExit {
            spawned: true,
            provider_released: true,
            reaped: true,
            containment_verified_empty: true,
            success: status.success(),
            code: status.code(),
            signal: None,
        },
        Err(error) => {
            fatal_error = Some(format!("Claude wait failed: {error}"));
            CodexAppServerProcessExit {
                spawned: true,
                provider_released: true,
                reaped: false,
                containment_verified_empty: true,
                success: false,
                code: None,
                signal: None,
            }
        }
    };

    let cancelled = cancellation.is_cancelled();
    let outcome = if cancelled {
        AgentSessionOutcome::Cancelled
    } else if fatal_error.is_some() || !process.success {
        AgentSessionOutcome::Failed
    } else {
        AgentSessionOutcome::Clean
    };
    let cause = if cancelled {
        CodexAppServerExitCause::Cancelled
    } else if fatal_error.is_some() {
        CodexAppServerExitCause::ProviderFailed
    } else if !process.success {
        CodexAppServerExitCause::ProviderFailed
    } else {
        CodexAppServerExitCause::Clean
    };

    if !cancelled && fatal_error.is_none() {
        let _ = sink.try_emit(AgentEventKind::TurnCompleted { turn_id }, None);
    }
    let _ = sink.try_emit(AgentEventKind::SessionEnded { outcome }, None);
    {
        let mut snapshot = view.lock();
        snapshot.phase = match outcome {
            AgentSessionOutcome::Clean => CodexAppServerPhase::Ended,
            AgentSessionOutcome::Failed => CodexAppServerPhase::Failed,
            AgentSessionOutcome::Cancelled => CodexAppServerPhase::Ended,
        };
        if saw_result || matches!(outcome, AgentSessionOutcome::Clean) {
            snapshot.completed_turns = 1;
        }
        if let Some(detail) = &fatal_error {
            snapshot.last_error = Some(detail.clone());
        }
    }
    *exit_report.lock() = Some(CodexAppServerExitReport {
        outcome,
        cause,
        detail: fatal_error,
        process,
        critical_event_delivery_failed: false,
        stderr_tail: stderr_tail.lock().clone(),
    });
    sink.close();
    while command_receiver.try_recv().is_ok() {}
    let _ = command_receiver;
}

fn spawn_claude(argv: &[String], worktree_path: &PathBuf) -> Result<Child, String> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "Claude launch argv is empty".to_string())?;
    Command::new(program)
        .args(args)
        .current_dir(worktree_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .spawn()
        .map_err(|error| format!("failed to spawn Claude: {error}"))
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let started = std::time::Instant::now();
    while started.elapsed() < TERMINATE_GRACE {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => return,
        }
    }
    let _ = child.wait();
}

fn publish_failure(
    sink: &AgentEventSink,
    view: &Mutex<CodexAppServerViewSnapshot>,
    exit_report: &Mutex<Option<CodexAppServerExitReport>>,
    cause: CodexAppServerExitCause,
    detail: String,
    process: CodexAppServerProcessExit,
) {
    {
        let mut snapshot = view.lock();
        snapshot.phase = CodexAppServerPhase::Failed;
        snapshot.last_error = Some(detail.clone());
    }
    let _ = sink.try_emit(AgentEventKind::Error { fatal: true }, Some(detail.clone()));
    let _ = sink.try_emit(
        AgentEventKind::SessionEnded {
            outcome: AgentSessionOutcome::Failed,
        },
        Some(detail.clone()),
    );
    *exit_report.lock() = Some(CodexAppServerExitReport {
        outcome: AgentSessionOutcome::Failed,
        cause,
        detail: Some(detail),
        process,
        critical_event_delivery_failed: false,
        stderr_tail: String::new(),
    });
    sink.close();
}

fn append_agent_text(snapshot: &mut CodexAppServerViewSnapshot, text: &str) {
    append_bounded(&mut snapshot.agent_text, text.as_bytes(), AGENT_TEXT_MAX_BYTES);
    if snapshot.agent_text.len() >= AGENT_TEXT_MAX_BYTES {
        snapshot.agent_text_truncated = true;
    }
}

fn append_bounded(buffer: &mut String, bytes: &[u8], max_bytes: usize) {
    let remaining = max_bytes.saturating_sub(buffer.len());
    if remaining == 0 {
        return;
    }
    let take = remaining.min(bytes.len());
    buffer.push_str(&String::from_utf8_lossy(&bytes[..take]));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_init_assistant_result_and_errors() {
        assert_eq!(
            parse_stream_json_line(
                r#"{"type":"system","subtype":"init","session_id":"sess-1"}"#
            ),
            ClaudeStreamSignal::SessionInit {
                session_id: Some("sess-1".into())
            }
        );
        assert_eq!(
            parse_stream_json_line(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#
            ),
            ClaudeStreamSignal::TextDelta("hello".into())
        );
        assert_eq!(
            parse_stream_json_line(
                r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"!"}}"#
            ),
            ClaudeStreamSignal::TextDelta("!".into())
        );
        assert_eq!(
            parse_stream_json_line(
                r#"{"type":"result","subtype":"success","result":"done"}"#
            ),
            ClaudeStreamSignal::Result {
                success: true,
                result_text: Some("done".into())
            }
        );
        assert_eq!(
            parse_stream_json_line(r#"{"type":"result","subtype":"error","error":"nope"}"#),
            ClaudeStreamSignal::Error("nope".into())
        );
    }

    #[test]
    fn print_argv_pins_stream_json_flags() {
        let argv = claude_print_argv(&["/usr/bin/claude".into()], "fix the build").unwrap();
        assert_eq!(
            argv,
            vec![
                "/usr/bin/claude",
                "-p",
                "--verbose",
                "--output-format",
                "stream-json",
                "--dangerously-skip-permissions",
                "fix the build",
            ]
        );
    }
}
