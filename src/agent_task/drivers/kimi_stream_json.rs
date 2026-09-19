//! Minimal Kimi Code adapter using `-p` stream-json.
//!
//! Print mode creates sessions with `permission: "auto"` (no `--yolo`; that
//! flag conflicts with `--prompt`). Failures should fall back to the opaque
//! PTY launcher. This MVP mirrors Claude's print/stream-json path: no private
//! home, no cgroup containment (a private process group only), one prompt
//! turn per session. Kimi reports failures on stderr with a non-zero exit, so
//! the shared worker in [`super::print_stream`] folds the last stderr lines
//! into the failure detail.

use super::print_stream::{bounded_detail, PrintProviderSpec, PrintSignal, PrintStreamDriver};
use crate::agent_task::driver::{AgentCommand, AgentDriver, AgentDriverError, AgentStartRequest};
use crate::agent_task::drivers::codex_app_server::{
    CodexAppServerExitReport, CodexAppServerPhase, CodexAppServerViewSnapshot,
};
use crate::agent_task::{AgentEvent, AgentProvider};
use serde_json::Value;
use std::path::PathBuf;

/// One normalized signal derived from a Kimi Code stream-json line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KimiStreamSignal {
    SessionInit {
        session_id: Option<String>,
    },
    TextDelta(String),
    /// Non-fatal progress such as `turn.step.retrying`.
    Status(String),
    Result {
        success: bool,
        result_text: Option<String>,
    },
    Error(String),
    Ignored,
}

/// Parse one complete Kimi Code stream-json / NDJSON line.
///
/// Prompt mode emits OpenAI-shaped chat lines plus meta / goal summaries:
/// - `{"role":"meta","type":"system.version",…}`
/// - `{"role":"meta","type":"session.resume_hint","session_id":…}`
/// - `{"role":"assistant","content":"…","tool_calls":[…]}`
/// - `{"role":"tool","tool_call_id":…,"content":"…"}`
/// - `{"type":"goal.summary","status":"complete"|…}`
pub fn parse_stream_json_line(line: &str) -> KimiStreamSignal {
    let line = line.trim();
    if line.is_empty() {
        return KimiStreamSignal::Ignored;
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return KimiStreamSignal::Ignored;
    };

    if value.get("type").and_then(Value::as_str) == Some("goal.summary") {
        return parse_goal_summary(&value);
    }

    let role = value.get("role").and_then(Value::as_str).unwrap_or("");
    match role {
        "meta" => {
            let meta_type = value.get("type").and_then(Value::as_str).unwrap_or("");
            match meta_type {
                "session.resume_hint" => {
                    let session_id = value
                        .get("session_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    KimiStreamSignal::SessionInit { session_id }
                }
                "turn.step.retrying" => {
                    let detail = value
                        .get("error_message")
                        .and_then(Value::as_str)
                        .or_else(|| value.get("error_name").and_then(Value::as_str))
                        .unwrap_or("Kimi turn step is retrying");
                    KimiStreamSignal::Status(bounded_detail(&format!("Kimi is retrying: {detail}")))
                }
                _ => KimiStreamSignal::Ignored,
            }
        }
        "assistant" => {
            if let Some(text) = value.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    return KimiStreamSignal::TextDelta(text.to_owned());
                }
            }
            // Tool-call-only assistant flushes still advance the turn; ignore.
            KimiStreamSignal::Ignored
        }
        "tool" => KimiStreamSignal::Ignored,
        _ => KimiStreamSignal::Ignored,
    }
}

fn parse_goal_summary(value: &Value) -> KimiStreamSignal {
    let status = value.get("status").and_then(Value::as_str);
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match status {
        Some("complete") => KimiStreamSignal::Result {
            success: true,
            result_text: reason,
        },
        Some(other) => {
            let detail = reason.unwrap_or_else(|| format!("Kimi goal ended with status {other}"));
            KimiStreamSignal::Error(detail)
        }
        None => KimiStreamSignal::Result {
            success: true,
            result_text: reason,
        },
    }
}

/// Build the print-mode argv for one Kimi Code native turn.
///
/// `--yolo` / `--auto` cannot combine with `--prompt`; print mode itself
/// creates the session with `permission: "auto"`.
pub fn kimi_print_argv(executable_argv: &[String], prompt: &str) -> Result<Vec<String>, String> {
    if executable_argv.is_empty() {
        return Err("Kimi launch argv is empty".into());
    }
    if prompt.trim().is_empty() {
        return Err("Kimi print prompt is empty".into());
    }
    let mut argv = executable_argv.to_vec();
    argv.extend([
        "-p".to_string(),
        prompt.to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
    ]);
    Ok(argv)
}

static KIMI_SPEC: PrintProviderSpec = PrintProviderSpec {
    provider: AgentProvider::Kimi,
    label: "Kimi",
    thread_suffix: "kimi-stream-json",
    env_remove: &[],
    // `session.resume_hint` is printed only after a successful turn, so the
    // session starts immediately and the id is recorded in the view only.
    session_id_first: false,
    argv: kimi_print_argv,
    parse: kimi_print_signal,
};

fn kimi_print_signal(line: &str) -> PrintSignal {
    match parse_stream_json_line(line) {
        KimiStreamSignal::SessionInit {
            session_id: Some(session_id),
        } => PrintSignal::SessionId(session_id),
        KimiStreamSignal::SessionInit { session_id: None } => PrintSignal::Ignored,
        KimiStreamSignal::TextDelta(text) => PrintSignal::Text(text),
        KimiStreamSignal::Status(status) => PrintSignal::Status(status),
        KimiStreamSignal::Result {
            success,
            result_text,
        } => PrintSignal::Finished {
            success,
            text: result_text,
            detail: None,
        },
        KimiStreamSignal::Error(detail) => PrintSignal::Fatal(detail),
        KimiStreamSignal::Ignored => PrintSignal::Ignored,
    }
}

/// Native Kimi Code driver (print / stream-json MVP).
pub struct KimiStreamJsonDriver(PrintStreamDriver);

impl KimiStreamJsonDriver {
    pub fn new(launch_argv: Vec<String>, worktree_path: PathBuf) -> Self {
        Self(PrintStreamDriver::new(
            &KIMI_SPEC,
            launch_argv,
            worktree_path,
        ))
    }

    pub fn view_snapshot(&self) -> CodexAppServerViewSnapshot {
        self.0.view_snapshot()
    }

    pub fn phase(&self) -> CodexAppServerPhase {
        self.0.phase()
    }

    pub fn take_exit_report(&self) -> Option<CodexAppServerExitReport> {
        self.0.take_exit_report()
    }

    pub fn worker_is_finished(&self) -> bool {
        self.0.worker_is_finished()
    }

    pub fn join_finished_worker(&mut self) -> Result<bool, AgentDriverError> {
        self.0.join_finished_worker()
    }
}

impl AgentDriver for KimiStreamJsonDriver {
    fn provider(&self) -> AgentProvider {
        AgentProvider::Kimi
    }

    fn start(&mut self, request: AgentStartRequest) -> Result<(), AgentDriverError> {
        self.0.start(request)
    }

    fn send(&mut self, command: AgentCommand) -> Result<(), AgentDriverError> {
        self.0.send(command)
    }

    fn cancel(&mut self) {
        self.0.cancel();
    }

    fn try_next_event(&mut self) -> Result<Option<AgentEvent>, AgentDriverError> {
        self.0.try_next_event()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_meta_assistant_goal_and_errors() {
        assert_eq!(
            parse_stream_json_line(r#"{"role":"meta","type":"system.version","version":"0.36.1"}"#),
            KimiStreamSignal::Ignored
        );
        assert_eq!(
            parse_stream_json_line(
                r#"{"role":"meta","type":"session.resume_hint","session_id":"sess-k","command":"kimi -r sess-k","content":"To resume"}"#
            ),
            KimiStreamSignal::SessionInit {
                session_id: Some("sess-k".into())
            }
        );
        assert_eq!(
            parse_stream_json_line(r#"{"role":"assistant","content":"pong"}"#),
            KimiStreamSignal::TextDelta("pong".into())
        );
        assert_eq!(
            parse_stream_json_line(r#"{"role":"tool","tool_call_id":"call-1","content":"ok"}"#),
            KimiStreamSignal::Ignored
        );
        assert_eq!(
            parse_stream_json_line(
                r#"{"type":"goal.summary","goalId":"g1","status":"complete","reason":null}"#
            ),
            KimiStreamSignal::Result {
                success: true,
                result_text: None
            }
        );
        assert_eq!(
            parse_stream_json_line(
                r#"{"type":"goal.summary","goalId":"g1","status":"blocked","reason":"needs input"}"#
            ),
            KimiStreamSignal::Error("needs input".into())
        );
    }

    #[test]
    fn print_argv_pins_stream_json_flags() {
        let argv = kimi_print_argv(&["/usr/bin/kimi".into()], "fix the build").unwrap();
        assert_eq!(
            argv,
            vec![
                "/usr/bin/kimi",
                "-p",
                "fix the build",
                "--output-format",
                "stream-json",
            ]
        );
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn step_retry_is_status_not_assistant_text() {
        assert_eq!(
            parse_stream_json_line(
                r#"{"role":"meta","type":"turn.step.retrying","failed_attempt":1,"next_attempt":2,"max_attempts":3,"delay_ms":100,"error_name":"APIConnectionError","error_message":"socket hang up","status_code":null}"#
            ),
            KimiStreamSignal::Status("Kimi is retrying: socket hang up".into())
        );
    }
}

#[cfg(all(test, unix))]
mod driver_tests {
    use super::super::print_stream::test_support::*;
    use super::*;
    use crate::agent_task::{AgentEventKind, AgentSessionOutcome, TaskStatus};
    use std::time::{Duration, Instant};

    fn recorded_success() -> String {
        [
            r#"{"role":"meta","type":"system.version","version":"0.36.1"}"#,
            r#"{"role":"assistant","content":"Let me look at the build.","tool_calls":[{"type":"function","id":"call_1","function":{"name":"Shell","arguments":"{\"command\":\"cargo build\"}"}}]}"#,
            r#"{"role":"tool","tool_call_id":"call_1","content":"error[E0432]: unresolved import"}"#,
            r#"{"role":"meta","type":"turn.step.retrying","failed_attempt":1,"next_attempt":2,"max_attempts":3,"delay_ms":100,"error_name":"APIConnectionError","error_message":"socket hang up","status_code":null}"#,
            r#"{"role":"assistant","content":"Fixed the import."}"#,
            r#"{"role":"meta","type":"session.resume_hint","session_id":"sess-k1","command":"kimi -r sess-k1","content":"To resume this session: kimi -r sess-k1"}"#,
        ]
        .join("\n")
            + "\n"
    }

    #[test]
    fn recorded_kimi_run_reaches_ready_for_review_with_one_session_start() {
        let dir = TempDir::new("kimi-ok");
        let cli = fake_cli(&dir.0, &recorded_success(), "exit 0");
        let (mut manager, task_id, stream) = native_task(AgentProvider::Kimi, &dir.0);
        let mut driver = KimiStreamJsonDriver::new(fake_cli_argv(&cli), dir.0.clone());
        driver
            .start(start_request(AgentProvider::Kimi, stream, &dir.0))
            .unwrap();
        let events = drive_to_close(&mut driver, &mut manager, Duration::from_secs(20));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind(), AgentEventKind::SessionStarted { .. }))
                .count(),
            1,
            "{events:?}"
        );
        assert!(matches!(
            events.last().map(AgentEvent::kind),
            Some(AgentEventKind::SessionEnded {
                outcome: AgentSessionOutcome::Clean
            })
        ));
        assert_eq!(
            manager.get(task_id).unwrap().status,
            TaskStatus::ReadyForReview
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !driver.join_finished_worker().unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let view = driver.view_snapshot();
        // The resume hint arrives after the turn: recorded, not re-announced.
        assert_eq!(view.provider_thread_id.as_deref(), Some("sess-k1"));
        assert_eq!(
            view.agent_text,
            "Let me look at the build.Fixed the import."
        );
        let report = driver.take_exit_report().unwrap();
        assert_eq!(report.outcome, AgentSessionOutcome::Clean);
        assert!(report.process.containment_verified_empty);
    }

    #[test]
    fn kimi_failure_reports_last_stderr_lines() {
        let dir = TempDir::new("kimi-err");
        let cli = fake_cli(
            &dir.0,
            "{\"role\":\"meta\",\"type\":\"system.version\",\"version\":\"0.36.1\"}\n",
            "echo 'Warning: config diagnostics' >&2\nprintf 'Error: \\033[31mLLM provider error: 401 Invalid Authentication\\033[0m\\n' >&2\nexit 1",
        );
        let (mut manager, task_id, stream) = native_task(AgentProvider::Kimi, &dir.0);
        let mut driver = KimiStreamJsonDriver::new(fake_cli_argv(&cli), dir.0.clone());
        driver
            .start(start_request(AgentProvider::Kimi, stream, &dir.0))
            .unwrap();
        drive_to_close(&mut driver, &mut manager, Duration::from_secs(20));
        let task = manager.get(task_id).unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        let detail = task.status_detail.clone().unwrap_or_default();
        assert!(detail.contains("401 Invalid Authentication"), "{detail}");
        assert!(!detail.contains('\u{1b}'), "{detail}");
        while !driver.join_finished_worker().unwrap() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let report = driver.take_exit_report().unwrap();
        assert_eq!(report.outcome, AgentSessionOutcome::Failed);
        assert_eq!(report.process.code, Some(1));
        assert!(report
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("401 Invalid Authentication")));
    }
}
