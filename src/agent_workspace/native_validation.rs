//! Explicit, host-owned validation execution. Commands never enter descriptors.
use super::host::{self, RuntimeHandle, RuntimePty};
use super::model::ConversationId;
use super::ownership::{Lease, NativeSessionLease, Paths};
use crate::agent_task::{prepare_task_validation, AgentTask, PreparedTaskValidation};
use crate::native_pty_launch::launch_hosted_pty;
use crate::screen_replay::{resync_ring_head, ScreenReplay};
use std::os::fd::AsFd;
use std::process::Command;
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
    let lease = Lease::acquire(&paths.lease(&id))?;
    let mut command = Command::new(&prepared.argv[0]);
    command
        .args(&prepared.argv[1..])
        .env("TERM", "xterm-256color")
        .env("BASH_ENV", "/dev/null")
        .env("ENV", "/dev/null")
        .env("ZDOTDIR", "/dev/null");
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
    let (master, child) = launch_hosted_pty(
        &command,
        Some(prepared.prepared.pinned_cwd.as_fd()),
        30,
        100,
        &[lease.as_fd(), task_lease.as_ref().as_fd()],
    )
    .map_err(|e| format!("Could not start validation: {e}"))?;
    host::register_pty(
        id,
        (100, 30),
        "jterm-validation-pty",
        "Host-owned validation PTY",
        RuntimePty {
            master,
            child,
            lease,
            guard: (prepared, task_lease),
        },
    )
    .map_err(|e| e.to_string())
}

pub(super) fn output(runtime: &RuntimeHandle, redact: bool) -> (String, bool) {
    let capture = runtime.output.lock();
    let raw = capture.raw.iter().copied().collect::<Vec<_>>();
    let mut replay = ScreenReplay::with_budget(100, 30, 64 * 1024);
    let mut truncated = capture.truncated;
    replay.feed(if truncated {
        resync_ring_head(&raw)
    } else {
        &raw
    });
    let mut text = replay.finish().to_plain();
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
