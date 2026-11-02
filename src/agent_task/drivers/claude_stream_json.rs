//! Claude Code adapter using exact-ID `--print` stream-json turns.
//!
//! This is deliberately weaker than the Codex app-server path: no private home,
//! no cgroup containment (a private process group only). Conversation mode
//! holds a profile-bound lease across turns; each child is reaped before the
//! next exact-ID resume. Interactive tool permissions remain a PTY capability.
//! Process ownership and the lifecycle contract live in
//! `super::print_stream`.

use super::print_stream::{
    bounded_detail, parse_json_record, PrintProviderSpec, PrintSignal, PrintStreamDriver,
};
use crate::agent_task::driver::{AgentCommand, AgentDriver, AgentDriverError, AgentStartRequest};
use crate::agent_task::drivers::codex_app_server::{
    CodexAppServerExitReport, CodexAppServerPhase, CodexAppServerViewSnapshot,
};
use crate::agent_task::{AgentEvent, AgentProvider};
use serde_json::Value;
use std::path::PathBuf;

/// At most this many entries of a result's `errors` array are reported.
const RESULT_ERRORS_MAX: usize = 4;

/// One normalized signal derived from a Claude Code stream-json line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaudeStreamSignal {
    SessionInit {
        session_id: Option<String>,
    },
    TextDelta(String),
    /// Non-fatal progress such as `system/api_retry`.
    Status(String),
    Result {
        success: bool,
        result_text: Option<String>,
    },
    Error(String),
    Ignored,
}

/// Parse one complete Claude Code stream-json line.
///
/// Only a `result` whose `is_error` is true or whose subtype starts with
/// `error` (`error_max_turns`, `error_during_execution`, ...) and a top-level
/// `error` record are fatal. Other `system` records, including transient
/// `api_retry` notices that carry an `error` field, are informational.
/// Oversized records, duplicate JSON members and private decoder escape
/// members are errors before any event can change session state.
pub fn parse_stream_json_line(line: &str) -> ClaudeStreamSignal {
    let value = match parse_json_record(line) {
        Ok(Some(value)) => value,
        Ok(None) => return ClaudeStreamSignal::Ignored,
        Err(detail) => return ClaudeStreamSignal::Error(format!("Claude {detail}")),
    };
    let Some(event_type) = value.get("type").and_then(Value::as_str) else {
        return ClaudeStreamSignal::Ignored;
    };
    match event_type {
        "system" => match value.get("subtype").and_then(Value::as_str).unwrap_or("") {
            "init" => ClaudeStreamSignal::SessionInit {
                session_id: value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
            "api_retry" => ClaudeStreamSignal::Status(api_retry_status(&value)),
            _ => ClaudeStreamSignal::Ignored,
        },
        "assistant" => extract_assistant_text(&value)
            .map(ClaudeStreamSignal::TextDelta)
            .unwrap_or(ClaudeStreamSignal::Ignored),
        "stream_event" | "content_block_delta" => extract_text_delta(&value)
            .map(ClaudeStreamSignal::TextDelta)
            .unwrap_or(ClaudeStreamSignal::Ignored),
        "result" => {
            if value
                .get("permission_denials")
                .and_then(Value::as_array)
                .is_some_and(|denials| !denials.is_empty())
            {
                return ClaudeStreamSignal::Error(
                    "Claude requires tool permissions that this print channel cannot request; continue in Terminal to review them".into(),
                );
            }
            let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or("");
            let is_error = value.get("is_error").and_then(Value::as_bool) == Some(true);
            let failed = is_error || subtype.starts_with("error");
            let result_text = value
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| extract_assistant_text(&value));
            if !failed {
                return ClaudeStreamSignal::Result {
                    success: true,
                    result_text,
                };
            }
            ClaudeStreamSignal::Error(result_failure_detail(&value, subtype, result_text))
        }
        "error" => {
            let detail = value
                .get("error")
                .and_then(|error| {
                    error.as_str().map(str::to_owned).or_else(|| {
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                })
                .or_else(|| {
                    value
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| "Claude stream error".to_owned());
            ClaudeStreamSignal::Error(bounded_detail(&detail))
        }
        _ => ClaudeStreamSignal::Ignored,
    }
}

fn api_retry_status(value: &Value) -> String {
    let error = value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("transient error");
    let attempt = value.get("attempt").and_then(Value::as_u64);
    let max_retries = value.get("max_retries").and_then(Value::as_u64);
    let status = match (attempt, max_retries) {
        (Some(attempt), Some(max)) => format!("Claude API retry {attempt}/{max}: {error}"),
        (Some(attempt), None) => format!("Claude API retry {attempt}: {error}"),
        _ => format!("Claude API retry: {error}"),
    };
    bounded_detail(&status)
}

/// Build the user-visible reason for a failed `result`: its `errors[]`
/// strings (bounded), else a legacy `error` string, else the result text,
/// else the subtype.
fn result_failure_detail(value: &Value, subtype: &str, result_text: Option<String>) -> String {
    let errors: Vec<&str> = value
        .get("errors")
        .and_then(Value::as_array)
        .map(|errors| {
            errors
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|error| !error.is_empty())
                .take(RESULT_ERRORS_MAX)
                .collect()
        })
        .unwrap_or_default();
    let detail = if !errors.is_empty() {
        errors.join("; ")
    } else if let Some(error) = value.get("error").and_then(Value::as_str) {
        error.to_owned()
    } else if let Some(text) = result_text.filter(|text| !text.trim().is_empty()) {
        text
    } else if !subtype.is_empty() {
        format!("Claude print session ended with {subtype}")
    } else {
        "Claude print session failed".to_owned()
    };
    let detail = if subtype.starts_with("error") && !detail.contains(subtype) {
        format!("{subtype}: {detail}")
    } else {
        detail
    };
    bounded_detail(&detail)
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
    let delta = value
        .get("delta")
        .or_else(|| value.get("event").and_then(|event| event.get("delta")))?;
    if delta.get("type").and_then(Value::as_str) == Some("text_delta")
        || delta.get("type").and_then(Value::as_str).is_none()
    {
        return delta.get("text").and_then(Value::as_str).map(str::to_owned);
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
        "--permission-mode".to_string(),
        "dontAsk".to_string(),
        prompt.to_string(),
    ]);
    Ok(argv)
}

/// Resume only an exact, validated conversation identity, never "latest".
pub fn claude_resume_argv(
    executable_argv: &[String],
    prompt: &str,
    session_id: &str,
) -> Result<Vec<String>, String> {
    if executable_argv.is_empty() {
        return Err("Claude launch argv is empty".into());
    }
    let id = uuid::Uuid::parse_str(session_id)
        .map_err(|_| "Claude resume requires an exact UUID".to_owned())?;
    if id.to_string() != session_id {
        return Err("Claude resume requires a canonical UUID".into());
    }
    let mut launch = executable_argv.to_vec();
    launch.extend(["--resume".into(), session_id.to_owned()]);
    claude_print_argv(&launch, prompt)
}

static CLAUDE_SPEC: PrintProviderSpec = PrintProviderSpec {
    provider: AgentProvider::Claude,
    label: "Claude",
    thread_suffix: "claude-stream-json",
    env_remove: &["CLAUDE_CODE_ENTRYPOINT"],
    // `system/init` (carrying `session_id`) is Claude's first stdout record.
    session_id_first: true,
    argv: claude_print_argv,
    resume_argv: Some(claude_resume_argv),
    parse: claude_print_signal,
};

fn claude_print_signal(line: &str) -> PrintSignal {
    match parse_stream_json_line(line) {
        ClaudeStreamSignal::SessionInit {
            session_id: Some(session_id),
        } => PrintSignal::SessionId(session_id),
        ClaudeStreamSignal::SessionInit { session_id: None } => PrintSignal::Ignored,
        ClaudeStreamSignal::TextDelta(text) => PrintSignal::Text(text),
        ClaudeStreamSignal::Status(status) => PrintSignal::Status(status),
        ClaudeStreamSignal::Result {
            success,
            result_text,
        } => PrintSignal::Finished {
            success,
            text: result_text,
            detail: None,
        },
        ClaudeStreamSignal::Error(detail) => PrintSignal::Fatal(detail),
        ClaudeStreamSignal::Ignored => PrintSignal::Ignored,
    }
}

/// Native Claude Code driver with bounded exact-ID conversation continuation.
pub struct ClaudeStreamJsonDriver(PrintStreamDriver);

impl ClaudeStreamJsonDriver {
    pub fn new(launch_argv: Vec<String>, worktree_path: PathBuf) -> Self {
        Self(PrintStreamDriver::new(
            &CLAUDE_SPEC,
            launch_argv,
            worktree_path,
        ))
    }

    /// Keep the native session open at review points for user follow-up turns.
    /// The profile is resolved and pinned on the worker before any CLI starts.
    pub fn new_conversation(launch_argv: Vec<String>, worktree_path: PathBuf) -> Self {
        let mut driver = Self::new(launch_argv, worktree_path);
        driver.0.enable_continuation(None);
        driver
    }

    pub(crate) fn configure_host(
        &mut self,
        context: crate::agent_workspace::NativeLaunchContext,
        lease: std::sync::Arc<crate::agent_workspace::NativeSessionLease>,
    ) {
        self.0.configure_host(context, lease);
    }

    pub fn view_snapshot(&self) -> CodexAppServerViewSnapshot {
        self.0.view_snapshot()
    }

    pub fn view_revision(&self) -> u64 {
        self.0.view_revision()
    }

    #[cfg(test)]
    pub(crate) fn mutate_view_for_test(
        &self,
        change: impl FnOnce(&mut CodexAppServerViewSnapshot),
    ) {
        self.0.mutate_view_for_test(change);
    }

    pub fn view_snapshot_with_revision(&self) -> (u64, CodexAppServerViewSnapshot) {
        self.0.view_snapshot_with_revision()
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

impl AgentDriver for ClaudeStreamJsonDriver {
    fn provider(&self) -> AgentProvider {
        AgentProvider::Claude
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
    fn parses_init_assistant_result_and_errors() {
        assert_eq!(
            parse_stream_json_line(r#"{"type":"system","subtype":"init","session_id":"sess-1"}"#),
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
                r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#
            ),
            ClaudeStreamSignal::Result {
                success: true,
                result_text: Some("done".into())
            }
        );
        assert_eq!(
            parse_stream_json_line(r#"{"type":"result","subtype":"error","error":"nope"}"#),
            ClaudeStreamSignal::Error("error: nope".into())
        );
    }

    #[test]
    fn transient_api_retry_is_status_not_fatal() {
        for error in ["overloaded", "rate_limit", "server_error"] {
            let line = format!(
                r#"{{"type":"system","subtype":"api_retry","attempt":2,"max_retries":10,"retry_delay_ms":1000,"error_status":529,"error":"{error}","session_id":"s","uuid":"u"}}"#
            );
            assert_eq!(
                parse_stream_json_line(&line),
                ClaudeStreamSignal::Status(format!("Claude API retry 2/10: {error}"))
            );
        }
        // Unknown system subtypes that happen to carry `error` stay benign.
        assert_eq!(
            parse_stream_json_line(r#"{"type":"system","subtype":"hook_response","error":"x"}"#),
            ClaudeStreamSignal::Ignored
        );
    }

    #[test]
    fn ambiguous_json_cannot_rewrite_a_result_or_session() {
        for line in [
            r#"{"type":"result","is_error":true,"is_error":false,"result":"done"}"#,
            r#"{"type":"result","is_error":true,"\u0069s_error":false}"#,
            r#"{"type":"system","subtype":"init","session_id":"one","session_id":"two"}"#,
            r#"{"type":"assistant","message":{"content":"one","content":"two"}}"#,
            r#"{"$serde_json::private::RawValue":"{\"type\":\"result\",\"is_error\":false}"}"#,
        ] {
            assert!(
                matches!(parse_stream_json_line(line), ClaudeStreamSignal::Error(_)),
                "ambiguous record acquired authority: {line}"
            );
        }
    }

    #[test]
    fn error_results_use_subtype_and_bounded_sanitised_errors() {
        assert_eq!(
            parse_stream_json_line(
                r#"{"type":"result","subtype":"error_max_turns","is_error":true,"num_turns":5,"errors":["Reached maximum number of turns (5)"]}"#
            ),
            ClaudeStreamSignal::Error(
                "error_max_turns: Reached maximum number of turns (5)".into()
            )
        );
        // `is_error` alone is fatal even with a success-looking subtype.
        assert!(matches!(
            parse_stream_json_line(
                r#"{"type":"result","subtype":"success","is_error":true,"result":"API Error: 401"}"#
            ),
            ClaudeStreamSignal::Error(detail) if detail == "API Error: 401"
        ));
        // An error subtype without `is_error` is fatal too.
        assert!(matches!(
            parse_stream_json_line(r#"{"type":"result","subtype":"error_during_execution"}"#),
            ClaudeStreamSignal::Error(detail)
                if detail == "Claude print session ended with error_during_execution"
        ));
        let hostile = format!(
            r#"{{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["a\u001b[31mb","c\nd","{}","e","f"]}}"#,
            "x".repeat(4096)
        );
        let ClaudeStreamSignal::Error(detail) = parse_stream_json_line(&hostile) else {
            panic!("error result was not fatal");
        };
        assert!(detail.starts_with("error_during_execution: a"), "{detail}");
        assert!(!detail.contains('\u{1b}') && !detail.contains('\n'));
        assert!(detail.len() <= super::super::print_stream::FAILURE_DETAIL_MAX_BYTES);
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
                "--permission-mode",
                "dontAsk",
                "fix the build",
            ]
        );
    }

    #[test]
    fn resume_argv_uses_one_exact_id_and_keeps_prompt_literal() {
        let id = "8f2c1e2a-5b7d-4c3e-9a1f-0d6e2b4c8a10";
        let prompt = "review $(touch /tmp/not-a-shell-command)\nthen explain";
        let argv = claude_resume_argv(&["/usr/bin/claude".into()], prompt, id).unwrap();
        assert_eq!(&argv[1..3], &["--resume", id]);
        assert_eq!(argv.last().map(String::as_str), Some(prompt));
        assert!(!argv
            .iter()
            .any(|arg| arg == "--continue" || arg == "--dangerously-skip-permissions"));
        for invalid in ["latest", "--help", "../session", ""] {
            assert!(claude_resume_argv(&["claude".into()], "review", invalid).is_err());
        }
    }

    #[test]
    fn result_with_permission_denials_requires_explicit_terminal_recovery() {
        let result = parse_stream_json_line(
            r#"{"type":"result","subtype":"success","permission_denials":[{"tool_name":"Bash"}],"result":"I could not run the command"}"#,
        );
        assert!(
            matches!(result, ClaudeStreamSignal::Error(detail) if detail.contains("continue in Terminal"))
        );
    }
}

#[cfg(all(test, unix))]
mod driver_tests {
    use super::super::print_stream::test_support::*;
    use super::*;
    use crate::agent_task::{AgentEventKind, AgentSessionOutcome, TaskStatus};
    use std::time::{Duration, Instant};

    const SESSION: &str = "8f2c1e2a-5b7d-4c3e-9a1f-0d6e2b4c8a10";

    fn drain_to_review(
        driver: &mut ClaudeStreamJsonDriver,
        manager: &mut crate::agent_task::TaskManager,
        task_id: crate::agent_task::TaskId,
        completed_turns: usize,
    ) -> Vec<AgentEvent> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut events = Vec::new();
        loop {
            match driver.try_next_event().unwrap() {
                Some(event) => {
                    manager.apply_agent_event(event.clone()).unwrap();
                    events.push(event);
                }
                None => {
                    let view = driver.view_snapshot();
                    if view.phase == CodexAppServerPhase::Ready
                        && view.completed_turns == completed_turns
                    {
                        assert_eq!(
                            manager.get(task_id).unwrap().status,
                            TaskStatus::ReadyForReview
                        );
                        return events;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "did not reach a review point: {view:?}"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    #[test]
    fn native_conversation_resumes_exact_id_and_holds_ownership_between_turns() {
        if crate::agent_workspace::isolated_lease_test(
            "agent_task::drivers::claude_stream_json::driver_tests::native_conversation_resumes_exact_id_and_holds_ownership_between_turns",
        ) {
            return;
        }
        let dir = TempDir::new("claude-multi-turn");
        let cli = dir.0.join("conversation.sh");
        std::fs::write(
            &cli,
            r#"id=
mode=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --session-id|--resume) mode="$1"; id="$2"; shift 2 ;;
    *) shift ;;
  esac
done
printf '%s|%s\n' "$mode" "$id" >> "$CLAUDE_CONFIG_DIR/invocations"
printf '{"type":"system","subtype":"init","session_id":"%s"}\n' "$id"
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"Reviewed."}]}}\n'
printf '{"type":"result","subtype":"success","is_error":false,"result":"Reviewed."}\n'
"#,
        )
        .unwrap();
        let argv = vec!["/bin/sh".into(), cli.display().to_string()];
        let (mut manager, task_id, stream) = native_task(AgentProvider::Claude, &dir.0);
        let mut driver = ClaudeStreamJsonDriver::new_conversation(argv.clone(), dir.0.clone());
        driver.0.enable_continuation(Some(dir.0.clone()));
        driver
            .start(start_request(AgentProvider::Claude, stream, &dir.0))
            .unwrap();
        let mut events = drain_to_review(&mut driver, &mut manager, task_id, 1);
        let id = driver.view_snapshot().provider_thread_id.unwrap();
        assert!(
            crate::agent_workspace::NativeSessionLease::acquire_at(
                dir.0.join("workspace-test-state"),
                crate::agent_sessions::SessionProvider::Claude,
                &dir.0,
                &id,
            )
            .is_err(),
            "idle native conversation lost its writer lease"
        );

        let prompt = crate::agent_task::driver::AgentPrompt::new("Check the second change");
        driver.send(AgentCommand::Prompt(prompt.clone())).unwrap();
        assert_eq!(
            driver.send(AgentCommand::Prompt(prompt)),
            Err(AgentDriverError::TurnActive)
        );
        events.extend(drain_to_review(&mut driver, &mut manager, task_id, 2));
        let view = driver.view_snapshot();
        assert_eq!(view.turn_history.len(), 1);
        assert_eq!(view.turn_history[0].agent_text, "Reviewed.");
        assert_eq!(view.displayed_turn_ordinal, Some(2));
        assert_eq!(
            view.displayed_follow_up_feedback.as_deref(),
            Some("Check the second change")
        );
        assert!(manager.has_active_agent_event_stream(task_id));
        driver.send(AgentCommand::FinishSession).unwrap();
        events.extend(drive_to_close(
            &mut driver,
            &mut manager,
            Duration::from_secs(10),
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !driver.join_finished_worker().unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind(), AgentEventKind::SessionStarted { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind(), AgentEventKind::TurnStarted { .. }))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind(), AgentEventKind::TurnCompleted { .. }))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind(), AgentEventKind::SessionEnded { .. }))
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(dir.0.join("invocations")).unwrap(),
            format!("--session-id|{id}\n--resume|{id}\n")
        );
        let report = driver.take_exit_report().unwrap();
        assert_eq!(report.outcome, AgentSessionOutcome::Clean);
        assert!(report.process.reaped && report.process.containment_verified_empty);
        assert!(crate::agent_workspace::NativeSessionLease::acquire_at(
            dir.0.join("workspace-test-state"),
            crate::agent_sessions::SessionProvider::Claude,
            &dir.0,
            &id,
        )
        .is_ok());
    }

    #[test]
    fn native_resume_rejects_wrong_provider_identity_before_any_next_turn() {
        let dir = TempDir::new("claude-wrong-identity");
        let cli = fake_cli(&dir.0, &recorded_success(), "exit 0");
        let (mut manager, task_id, stream) = native_task(AgentProvider::Claude, &dir.0);
        let mut driver =
            ClaudeStreamJsonDriver::new_conversation(fake_cli_argv(&cli), dir.0.clone());
        driver.0.enable_continuation(Some(dir.0.clone()));
        let mut request = start_request(AgentProvider::Claude, stream, &dir.0);
        request.resume_from = Some(
            crate::agent_task::ProviderSessionId::new(
                AgentProvider::Claude,
                uuid::Uuid::new_v4().to_string(),
            )
            .unwrap(),
        );
        driver.start(request).unwrap();
        let events = drive_to_close(&mut driver, &mut manager, Duration::from_secs(15));
        assert_eq!(manager.get(task_id).unwrap().status, TaskStatus::Failed);
        assert!(!events
            .iter()
            .any(|event| matches!(event.kind(), AgentEventKind::TurnCompleted { .. })));
        assert!(driver
            .view_snapshot()
            .last_error
            .unwrap()
            .contains("different session ID"));
    }

    fn recorded_success() -> String {
        [
            format!(r#"{{"type":"system","subtype":"init","cwd":"/work","session_id":"{SESSION}","tools":["Bash","Read","Edit"],"mcp_servers":[],"model":"claude-sonnet-5","permissionMode":"bypassPermissions","slash_commands":[],"apiKeySource":"none","output_style":"default","uuid":"u0"}}"#),
            format!(r#"{{"type":"system","subtype":"api_retry","attempt":1,"max_retries":10,"retry_delay_ms":500,"error_status":529,"error":"overloaded","session_id":"{SESSION}","uuid":"u1"}}"#),
            format!(r#"{{"type":"assistant","message":{{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-5","content":[{{"type":"text","text":"I'll check the build."}},{{"type":"tool_use","id":"toolu_1","name":"Bash","input":{{"command":"cargo build"}}}}],"stop_reason":null,"usage":{{"input_tokens":10,"output_tokens":5}}}},"parent_tool_use_id":null,"session_id":"{SESSION}","uuid":"u2"}}"#),
            format!(r#"{{"type":"user","message":{{"role":"user","content":[{{"tool_use_id":"toolu_1","type":"tool_result","content":"error[E0432]: unresolved import","is_error":false}}]}},"parent_tool_use_id":null,"session_id":"{SESSION}","uuid":"u3"}}"#),
            format!(r#"{{"type":"assistant","message":{{"id":"msg_2","type":"message","role":"assistant","model":"claude-sonnet-5","content":[{{"type":"text","text":"{}"}}],"stop_reason":"end_turn","usage":{{"input_tokens":10,"output_tokens":5}}}},"parent_tool_use_id":null,"session_id":"{SESSION}","uuid":"u4"}}"#, "Fixed the import. ".repeat(8 * 1024)),
            format!(r#"{{"type":"result","subtype":"success","is_error":false,"duration_ms":1234,"duration_api_ms":1000,"num_turns":3,"result":"Fixed the import.","session_id":"{SESSION}","total_cost_usd":0.01,"usage":{{"input_tokens":20,"output_tokens":10}},"permission_denials":[],"uuid":"u5"}}"#),
        ]
        .join("\n")
            + "\n"
    }

    #[test]
    fn recorded_claude_run_reaches_ready_for_review_with_one_session_start() {
        let dir = TempDir::new("claude-ok");
        let cli = fake_cli(&dir.0, &recorded_success(), "exit 0");
        let (mut manager, task_id, stream) = native_task(AgentProvider::Claude, &dir.0);
        let mut driver = ClaudeStreamJsonDriver::new(fake_cli_argv(&cli), dir.0.clone());
        driver
            .start(start_request(AgentProvider::Claude, stream, &dir.0))
            .unwrap();
        let events = drive_to_close(&mut driver, &mut manager, Duration::from_secs(20));

        let starts: Vec<_> = events
            .iter()
            .filter_map(|event| match event.kind() {
                AgentEventKind::SessionStarted {
                    provider_session_id,
                    ..
                } => Some(provider_session_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 1, "exactly one SessionStarted: {events:?}");
        assert_eq!(starts[0].as_ref().map(|id| id.opaque()), Some(SESSION));
        assert!(events
            .iter()
            .any(|event| matches!(event.kind(), AgentEventKind::Error { fatal: false })));
        assert!(!events
            .iter()
            .any(|event| matches!(event.kind(), AgentEventKind::Error { fatal: true })));
        // A 144 KiB assistant message is delivered as a bounded event and
        // retained in the view instead of being dropped.
        assert!(events.iter().all(|event| event.detail().is_none_or(
            |detail| detail.len() <= crate::agent_task::event::MAX_AGENT_EVENT_DETAIL_BYTES
        )));
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
        assert_eq!(view.provider_thread_id.as_deref(), Some(SESSION));
        assert!(view
            .agent_text
            .starts_with("I'll check the build.Fixed the import."));
        assert!(view.agent_text.len() > 64 * 1024);
        let report = driver.take_exit_report().unwrap();
        assert_eq!(report.outcome, AgentSessionOutcome::Clean);
        assert!(report.process.reaped && report.process.containment_verified_empty);
    }

    #[test]
    fn claude_error_result_fails_with_joined_errors() {
        let dir = TempDir::new("claude-err");
        let stdout = format!(
            "{}\n{}\n",
            format_args!(r#"{{"type":"system","subtype":"init","session_id":"{SESSION}"}}"#),
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"num_turns":5,"errors":["Reached maximum number of turns (5)"]}"#
        );
        let cli = fake_cli(&dir.0, &stdout, "exit 1");
        let (mut manager, task_id, stream) = native_task(AgentProvider::Claude, &dir.0);
        let mut driver = ClaudeStreamJsonDriver::new(fake_cli_argv(&cli), dir.0.clone());
        driver
            .start(start_request(AgentProvider::Claude, stream, &dir.0))
            .unwrap();
        drive_to_close(&mut driver, &mut manager, Duration::from_secs(20));
        let task = manager.get(task_id).unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert!(task
            .status_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("Reached maximum number of turns")));
    }

    #[test]
    fn cancel_during_silent_tool_call_kills_the_whole_group_promptly() {
        let dir = TempDir::new("claude-cancel");
        let pid_file = dir.0.join("tool.pid");
        let stdout =
            format!(r#"{{"type":"system","subtype":"init","session_id":"{SESSION}"}}"#) + "\n";
        // The "tool" is a grandchild that keeps stdout open; the CLI itself
        // then blocks silently, as it does during a long tool call.
        let cli = fake_cli(
            &dir.0,
            &stdout,
            &format!("sleep 60 & echo $! > '{}'\nsleep 60", pid_file.display()),
        );
        let (mut manager, task_id, stream) = native_task(AgentProvider::Claude, &dir.0);
        let mut driver = ClaudeStreamJsonDriver::new(fake_cli_argv(&cli), dir.0.clone());
        driver
            .start(start_request(AgentProvider::Claude, stream, &dir.0))
            .unwrap();
        let tool_pid = read_pid(&pid_file, Duration::from_secs(10));
        let cancelled_at = Instant::now();
        driver.cancel();
        drive_to_close(&mut driver, &mut manager, Duration::from_secs(10));
        assert!(
            cancelled_at.elapsed() < Duration::from_secs(5),
            "Stop took {:?}",
            cancelled_at.elapsed()
        );
        wait_until_not_live(tool_pid, Duration::from_secs(5));
        assert_eq!(manager.get(task_id).unwrap().status, TaskStatus::Cancelled);
        while !driver.join_finished_worker().unwrap() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let report = driver.take_exit_report().unwrap();
        assert_eq!(report.outcome, AgentSessionOutcome::Cancelled);
        assert!(report.process.reaped && report.process.containment_verified_empty);
    }

    #[test]
    fn dropping_a_running_driver_kills_its_process_group() {
        let dir = TempDir::new("claude-drop");
        let pid_file = dir.0.join("tool.pid");
        let cli = fake_cli(
            &dir.0,
            "",
            &format!("sleep 60 & echo $! > '{}'\nsleep 60", pid_file.display()),
        );
        let (_manager, _task_id, stream) = native_task(AgentProvider::Claude, &dir.0);
        let mut driver = ClaudeStreamJsonDriver::new(fake_cli_argv(&cli), dir.0.clone());
        driver
            .start(start_request(AgentProvider::Claude, stream, &dir.0))
            .unwrap();
        let tool_pid = read_pid(&pid_file, Duration::from_secs(10));
        let dropped_at = Instant::now();
        drop(driver);
        assert!(dropped_at.elapsed() < Duration::from_secs(5));
        wait_until_not_live(tool_pid, Duration::from_secs(5));
    }
}
