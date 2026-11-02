//! Explicit, host-owned validation execution. Commands never enter descriptors.
use super::host::{self, RuntimeHandle, RuntimeOutput};
use super::model::{ConversationId, RuntimeSnapshot, RuntimeState};
use super::ownership::{process_start, Lease, LeaseRecord, NativeSessionLease, Paths};
use crate::agent_task::{prepare_task_validation, AgentTask, PreparedTaskValidation};
use crate::screen_replay::{resync_ring_head, ScreenReplay};
use crate::supervised::SupervisedChild;
use crossbeam_channel::bounded;
use parking_lot::Mutex;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;

pub(super) struct PreparedValidation {
    pub prepared: PreparedTaskValidation,
    pub argv: Vec<String>,
    pub evidence: super::WorkspaceValidationEvidence,
}
pub(super) fn prepare(task: &AgentTask) -> Result<PreparedValidation, String> {
    let prepared = prepare_task_validation(task).map_err(|e| e.to_string())?;
    let shell = crate::host::resolve_configured_program(&prepared.source_shell, None)
        .ok_or("Validation source shell is no longer executable")?;
    let resolved = std::fs::canonicalize(&shell).map_err(|e| e.to_string())?;
    let family = resolved
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut argv = vec![resolved
        .to_str()
        .ok_or("Validation shell path is not UTF-8")?
        .to_owned()];
    match family.as_str() {
        "bash" => argv.extend(["--noprofile".into(), "--norc".into()]),
        "zsh" => argv.push("-f".into()),
        "fish" => argv.push("--no-config".into()),
        "jsh" => argv.push("--norc".into()),
        "sh" | "dash" | "ksh" | "ksh93" | "mksh" => {}
        _ => return Err("Unsupported source shell for isolated validation".into()),
    }
    argv.extend(["-c".into(), prepared.command.clone()]);
    let review = crate::agent_task::worktree::capture_task_review(task)?;
    let evidence = super::WorkspaceValidationEvidence {
        fingerprint: review.fingerprint,
        head: review.head,
        base_commit: review.base_commit,
        command: prepared.command.clone(),
        cwd: prepared.cwd.to_string_lossy().into_owned(),
        shell: prepared.source_shell.clone(),
        started_at_ms: review.captured_at_ms,
        completed_at_ms: None,
        unchanged_after_run: false,
        detail: None,
    };
    Ok(PreparedValidation {
        prepared,
        argv,
        evidence,
    })
}

pub(super) fn spawn(
    paths: &Paths,
    id: ConversationId,
    prepared: PreparedValidation,
    task_lease: Arc<NativeSessionLease>,
    search_path: Option<&str>,
) -> Result<RuntimeHandle, String> {
    let mut lease = Lease::acquire(&paths.lease(&id))?;
    let (master, slave) = host::open_pty(100, 30)?;
    let mut command = Command::new(&prepared.argv[0]);
    command
        .args(&prepared.argv[1..])
        .current_dir(prepared.prepared.pinned_cwd.proc_path())
        .env("TERM", "xterm-256color")
        .env("BASH_ENV", "/dev/null")
        .env("ENV", "/dev/null")
        .env("ZDOTDIR", "/dev/null")
        .stdin(Stdio::from(slave.try_clone().map_err(|e| e.to_string())?))
        .stdout(Stdio::from(slave.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(slave));
    if let Some(path) = search_path {
        command.env("PATH", path);
    }
    // A validation recipe does not inherit another provider's access grants.
    for key in [
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "KIMI_API_KEY",
    ] {
        command.env_remove(key);
    }
    lease.configure_child(&mut command)?;
    task_lease.configure_child(&mut command)?;
    let directory = prepared.prepared.pinned_cwd.as_raw_fd();
    // fchdir runs in the child while the pinned FD is still open. The user-
    // visible canonical path is never re-resolved after preflight.
    unsafe {
        command.pre_exec(move || {
            if libc::fchdir(directory) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = SupervisedChild::spawn_pty_session(&mut command)
        .map_err(|e| format!("Could not start validation: {e}"))?;
    let pid = child.process_group_id() as u32;
    let start = super::ownership::supervised_process_start(&child)
        .ok_or("Cannot record the owned validation process identity")?;
    let snapshot = RuntimeSnapshot {
        cleanup_verified: false,
        id: id.clone(),
        state: RuntimeState::Running,
        pid,
        process_start: start,
        epoch: uuid::Uuid::new_v4().to_string(),
        event_sequence: 0,
        cols: 100,
        rows: 30,
        exit_code: None,
        detail: "Host-owned validation PTY".into(),
    };
    let record = LeaseRecord {
        id,
        host_pid: std::process::id(),
        host_start: process_start(std::process::id()).ok_or("Cannot identify session host")?,
        runtime: snapshot.clone(),
    };
    lease.write(&record)?;
    let snapshot = Arc::new(Mutex::new(snapshot));
    let output = Arc::new(Mutex::new(RuntimeOutput::default()));
    let (control, rx) = bounded(128);
    let thread_snapshot = snapshot.clone();
    let thread_output = output.clone();
    std::thread::Builder::new()
        .name("jterm-validation-pty".into())
        .spawn(move || {
            let _task_lease = task_lease;
            let _prepared = prepared;
            host::runtime_loop(
                master,
                child,
                lease,
                record,
                rx,
                thread_snapshot,
                thread_output,
            );
        })
        .map_err(|e| e.to_string())?;
    Ok(RuntimeHandle {
        control,
        snapshot,
        output,
    })
}

pub(super) fn output(runtime: &RuntimeHandle, redact: bool) -> (String, bool) {
    let capture = runtime.output.lock();
    let raw = capture.raw.iter().copied().collect::<Vec<_>>();
    let mut replay = ScreenReplay::with_budget(100, 30, 64 * 1024);
    replay.feed(resync_ring_head(&raw));
    let mut text = replay.finish().to_plain();
    let mut truncated = capture.truncated;
    if redact {
        text = crate::redact::redact_secrets(&text);
    }
    if text.len() > 64 * 1024 {
        let mut start = text.len() - 64 * 1024;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text = text[start..].to_owned();
        truncated = true;
    }
    (text, truncated)
}
