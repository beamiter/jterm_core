//! The whole OSC-133-to-journal contract, exercised through the public API only.
//!
//! Every unit test around this contract necessarily stubs part of the path: the
//! parser tests stop at `CommandMeta`, and the writer tests hand-build a
//! lifecycle and call the append helper directly. Between them sits the seam
//! that actually broke — a terminal parses a `C`, keeps the token, and submits
//! its captured output through the global writer *after* the shell has already
//! appended its Finish. This drives that whole path once, with the exact bytes
//! a real jsh emits.
//!
//! It lives in `tests/` rather than beside the module because `submit` talks to
//! a process-global writer thread configured from the environment. An
//! integration test gets its own process, so it can own both without racing the
//! library suite.

use jterm_core::execution_journal as journal;
use jterm_core::parser::{Parser, ParserEvent};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

/// Captured from a real jsh under a PTY on 2026-09-05. Keeping the literal
/// bytes rather than reconstructing them is the point: this is the producer's
/// wire format, not our idea of it.
const REAL_COMMAND_START: &[u8] = b"\x1b]133;C;id=jsh-b8c6aba497355122-8942e-1a06f32c579-1;session_id=probe-session-1;seq=1;started_at_ms=1788571993465;cmdline_url=echo%20hello-journal;cwd_url=%2Fhome%2Fyj%2Fprojects%2Fjsh\x07";

/// The matching `D`, with the three Start-identity slots added on top of what
/// jsh really sends. jsh does not put them here; a hostile PTY writer would.
const FORGED_COMMAND_END: &[u8] = b"\x1b]133;D;0;id=jsh-b8c6aba497355122-8942e-1a06f32c579-1;session_id=probe-session-1;seq=1;started_at_ms=1788571993465;duration_ms=0\x07";

const EXECUTION_ID: &str = "jsh-b8c6aba497355122-8942e-1a06f32c579-1";
const SESSION_ID: &str = "probe-session-1";

fn parse(bytes: &[u8]) -> Vec<ParserEvent> {
    let mut parser = Parser::new();
    let mut events = Vec::new();
    parser.feed(bytes, &mut events);
    events
}

#[test]
fn a_terminal_output_submitted_after_the_shells_finish_reaches_the_record() {
    let directory = std::env::temp_dir().join(format!(
        "jterm-core-journal-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir(&directory).expect("private journal directory");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
        .expect("0700 journal directory");
    let path = directory.join("executions.jsonl");

    std::env::set_var("JSH_EXECUTION_JOURNAL", "1");
    std::env::set_var("JSH_EXECUTION_JOURNAL_PATH", &path);
    assert!(
        journal::output_capture_enabled(),
        "the capability gate must read the environment this test just set"
    );

    // 1. A complete `C` mints the capability.
    let meta = parse(REAL_COMMAND_START)
        .into_iter()
        .find_map(|event| match event {
            ParserEvent::CommandStart(meta) => Some(meta),
            _ => None,
        })
        .expect("the real packet yields a CommandStart");
    assert_eq!(meta.id.as_deref(), Some(EXECUTION_ID));
    assert_eq!(meta.session_id.as_deref(), Some(SESSION_ID));
    assert_eq!(meta.seq, Some(1));
    assert_eq!(meta.started_at_ms, Some(1_788_571_993_465));
    let lifecycle =
        journal::ExecutionLifecycle::from_command_meta(&meta).expect("a complete C mints a token");

    // 2. A `D` never does, however complete it looks. The identity slots are
    //    accepted only on `C`, so a PTY writer cannot forge a Start generation
    //    at the end of somebody else's command.
    let end_meta = parse(FORGED_COMMAND_END)
        .into_iter()
        .find_map(|event| match event {
            ParserEvent::CommandEnd { meta, .. } => Some(meta),
            _ => None,
        })
        .expect("the packet yields a CommandEnd");
    assert_eq!(end_meta.session_id, None, "D carries no Start identity");
    assert_eq!(end_meta.seq, None);
    assert_eq!(end_meta.started_at_ms, None);
    assert!(journal::ExecutionLifecycle::from_command_meta(&end_meta).is_none());

    // 3. The shell writes its Start and then, in the very next statement after
    //    emitting `D`, its Finish. This is the ordinary order — measured over a
    //    real ~/.local/state/jsh/executions.jsonl with 11,492 events, all 2,014
    //    lifecycles carrying terminal output are ordered Start, Finish, Output
    //    and none is ordered Start, Output, Finish.
    let mut file = std::fs::File::create(&path).expect("create journal");
    writeln!(
        file,
        r#"{{"event":"start","jsh_execution_version":1,"id":"{EXECUTION_ID}","session_id":"{SESSION_ID}","seq":1,"command":"echo hello-journal","command_truncated":false,"cwd":"/home/yj/projects/jsh","started_at_ms":1788571993465}}"#
    )
    .expect("start event");
    writeln!(
        file,
        r#"{{"event":"finish","jsh_execution_version":1,"id":"{EXECUTION_ID}","exit_code":0,"duration_ms":0,"cwd_after":"/home/yj/projects/jsh","ended_at_ms":1788571993470}}"#
    )
    .expect("finish event");
    drop(file);
    // jsh creates the journal private, and the writer refuses to append to one
    // that is not: a 0644 file fails with PermissionDenied before any lifecycle
    // question is asked.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("0600 journal");

    // 4. The terminal contributes its capture afterwards, and it is accepted.
    journal::submit(journal::CompletedExecution {
        lifecycle,
        output: "hello-journal".to_string(),
        output_available: true,
        truncated: false,
        total_bytes: 13,
    })
    .expect("submit is accepted");
    assert!(
        journal::flush(std::time::Duration::from_secs(10)),
        "the writer drains"
    );

    let text = std::fs::read_to_string(&path).expect("read journal");
    let events: Vec<&str> = text.lines().collect();
    assert_eq!(
        events.len(),
        3,
        "start, finish, then the terminal's output: {text}"
    );
    assert!(
        events[2].contains(r#""event":"output""#) && events[2].contains("hello-journal"),
        "the third event is the capture: {}",
        events[2]
    );

    // 5. And the reader folds all three onto one record — which is the thing a
    //    command sidebar actually renders.
    let load = journal::request_history(SESSION_ID.to_string()).expect("history request accepted");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let snapshot = loop {
        match load.try_snapshot().expect("the reader stays alive") {
            Some(snapshot) => break snapshot,
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            None => panic!("history read did not deliver a snapshot"),
        }
    };
    assert_eq!(snapshot.error, None);
    assert_eq!(snapshot.records.len(), 1, "one folded record");
    let record = &snapshot.records[0];
    assert_eq!(record.id, EXECUTION_ID);
    assert_eq!(record.command, "echo hello-journal");
    assert_eq!(record.exit_code, Some(0));
    assert_eq!(
        record.output.as_ref().expect("captured output").text,
        "hello-journal"
    );

    let _ = std::fs::remove_dir_all(&directory);
}
