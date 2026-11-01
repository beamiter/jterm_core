#![cfg(target_os = "linux")]
//! Real cross-process tests: the standalone binary owns a real PTY and
//! providers; terminal attachments are separately spawned client processes.
use jterm_core::agent_sessions::{AgentSession, SessionProvider, SessionStatus};
use jterm_core::agent_workspace::*;
use jterm_core::supervised::SupervisedChild;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const HOST: &str = env!("CARGO_BIN_EXE_jterm-sessiond");

struct Harness {
    root: PathBuf,
    state: PathBuf,
    host: Child,
    session: AgentSession,
    trace: PathBuf,
}
impl Harness {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("jw-{}", uuid::Uuid::new_v4().simple()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let state = root.join("state");
        let bin = root.join("bin");
        let profile = root.join("profile space/.codex");
        let cwd = root.join("project space");
        for dir in [&bin, &cwd, &profile.join("sessions/2026/11/01")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let id = uuid::Uuid::new_v4().to_string();
        let source = profile
            .join("sessions/2026/11/01")
            .join(format!("{id}.jsonl"));
        std::fs::write(&source, b"{}\n").unwrap();
        let fake = bin.join("codex");
        std::fs::write(&fake,b"#!/bin/sh\ntrap 'exit 0' TERM\nsh -c 'trap \"\" TERM; while :; do sleep 1; done' &\ntool=$!\nprintf 'boot %s %s\\n' \"$$\" \"$tool\" >> \"$JTERM_TEST_TRACE\"\nprintf 'HOME=%s CWD=%s\\n' \"$CODEX_HOME\" \"$PWD\"\nprintf 'READY %s\\n' \"$$\"\nwhile IFS= read -r line; do printf 'REPLY:%s\\n' \"$line\"; done\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let trace = root.join("trace");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let host = Command::new(HOST)
            .arg("--jterm-session-host")
            .env("JTERM_WORKSPACE_STATE_DIR", &state)
            .env("JTERM_TEST_TRACE", &trace)
            .env("PATH", path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(File::create(root.join("host-errors")).unwrap()))
            .spawn()
            .unwrap();
        let session = AgentSession {
            provider: SessionProvider::Codex,
            id,
            title: "Persistent test".into(),
            cwd,
            updated_at_ms: 0,
            status: SessionStatus::Unknown,
            preview: String::new(),
            branch: None,
            source,
            archived: false,
        };
        let h = Self {
            root,
            state,
            host,
            session,
            trace,
        };
        let client = h.client();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let result = request(&client, WorkspaceCommand::Refresh);
            if result.is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "host did not start: {:?}, {}",
                result,
                std::fs::read_to_string(h.root.join("host-errors")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        h
    }
    fn client(&self) -> WorkspaceClient {
        WorkspaceClient::for_state_dir(&self.state)
    }
    fn attach(&self) -> Terminal {
        let launch = managed_launch(&self.session).unwrap();
        let mut command = Command::new(HOST);
        command
            .args(launch.args)
            .current_dir(launch.cwd)
            .env("JTERM_WORKSPACE_STATE_DIR", &self.state);
        Terminal::spawn(command)
    }
    fn runtime(&self) -> RuntimeSnapshot {
        match request(
            &self.client(),
            WorkspaceCommand::Runtime {
                id: session_id(&self.session).unwrap(),
            },
        )
        .unwrap()
        {
            WorkspaceReply::Runtime { snapshot } => snapshot,
            _ => panic!("expected runtime"),
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        if let Ok(trace) = std::fs::read_to_string(&self.trace) {
            for line in trace.lines() {
                if let Some(pid) = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse::<i32>().ok())
                {
                    // Test providers use a unique fixture path; never signal a PID
                    // after it no longer identifies that fixture process.
                    if let Ok(args) = std::fs::read(format!("/proc/{pid}/cmdline")) {
                        if String::from_utf8_lossy(&args)
                            .contains(self.root.to_string_lossy().as_ref())
                        {
                            unsafe {
                                libc::kill(-pid, libc::SIGKILL);
                            }
                        }
                    }
                }
            }
        }
        let _ = self.host.kill();
        let _ = self.host.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Terminal {
    master: File,
    child: SupervisedChild,
    output: Vec<u8>,
}
impl Terminal {
    fn spawn(mut command: Command) -> Self {
        let mut master = -1;
        let mut slave = -1;
        let size = libc::winsize {
            ws_col: 100,
            ws_row: 30,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            },
            0
        );
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        command
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        let child = SupervisedChild::spawn_pty_session(&mut command).unwrap();
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }
        Self {
            master,
            child,
            output: Vec::new(),
        }
    }
    fn wait_for(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut bytes = [0; 8192];
        loop {
            match self.master.read(&mut bytes) {
                Ok(n) if n > 0 => self.output.extend_from_slice(&bytes[..n]),
                _ => {}
            }
            if String::from_utf8_lossy(&self.output).contains(text) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "did not see {text:?} in {:?}",
                String::from_utf8_lossy(&self.output)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn input(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }
    fn wait_exit(&mut self) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !self.child.root_has_exited().unwrap() {
            assert!(Instant::now() < deadline, "attachment did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        self.child
            .reap_after_group_kill()
            .unwrap()
            .code()
            .unwrap_or(-1)
    }
}

fn request(client: &WorkspaceClient, command: WorkspaceCommand) -> Result<WorkspaceReply, String> {
    let id = client.send(command)?;
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        for event in client.poll() {
            let WorkspaceEvent::Reply { request_id, result } = event;
            if request_id == id {
                return result;
            }
        }
        if Instant::now() > deadline {
            return Err("request timeout".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn process_live(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            s.rsplit_once(')')
                .map(|(_, rest)| !rest.trim_start().starts_with('Z'))
        })
        .unwrap_or(false)
}

#[test]
fn real_host_survives_client_exit_reattaches_once_and_stops_descendants() {
    let h = Harness::new();
    let mut first = h.attach();
    first.wait_for("READY");
    let original = h.runtime();
    assert_eq!(original.state, RuntimeState::Attached);
    first.wait_for("profile space/.codex");
    first.wait_for("project space");
    first.input(b"hello from first terminal\n");
    first.wait_for("REPLY:hello from first terminal");
    assert!(NativeSessionLease::acquire_at(
        &h.state,
        SessionProvider::Codex,
        &h.session.provider_home().unwrap(),
        &h.session.id
    )
    .is_err());
    let mut duplicate = h.attach();
    duplicate.wait_for("attached to another terminal");
    assert_ne!(duplicate.wait_exit(), 0);
    assert_eq!(
        std::fs::read_to_string(&h.trace).unwrap().lines().count(),
        1,
        "second attachment started another provider"
    );
    // Simulate closing the frontend's terminal without a graceful detach.
    first.child.reap_after_group_kill().unwrap();
    drop(first);
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.runtime().state == RuntimeState::Attached {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(h.runtime().pid, original.pid);
    assert!(process_live(original.pid));
    let mut resumed = h.attach();
    resumed.wait_for("REPLY:hello from first terminal");
    resumed.input(b"second connection\n");
    resumed.wait_for("REPLY:second connection");
    resumed.input(&[0x1d]);
    assert_eq!(resumed.wait_exit(), 0);
    assert!(process_live(original.pid));
    let mut last = h.attach();
    last.wait_for("REPLY:second connection");
    let trace = std::fs::read_to_string(&h.trace).unwrap();
    let tool: u32 = trace.split_whitespace().nth(2).unwrap().parse().unwrap();
    request(
        &h.client(),
        WorkspaceCommand::Stop {
            id: original.id.clone(),
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while h.runtime().state.is_live() {
        assert!(Instant::now() < deadline, "runtime did not stop");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(h.runtime().state, RuntimeState::Exited);
    assert!(!process_live(original.pid));
    assert!(!process_live(tool), "TERM-ignoring tool survived Stop");
    last.wait_exit();
    assert!(NativeSessionLease::acquire_at(
        &h.state,
        SessionProvider::Codex,
        &h.session.provider_home().unwrap(),
        &h.session.id
    )
    .is_ok());
}

#[test]
fn two_clients_share_store_and_enforce_exact_submission_receipts() {
    let h = Harness::new();
    let a = h.client();
    let b = h.client();
    let chat = match request(
        &a,
        WorkspaceCommand::CreateChat {
            title: "Cross-client API".into(),
        },
    )
    .unwrap()
    {
        WorkspaceReply::Chat { chat } => chat,
        _ => panic!(),
    };
    let receipt = match request(
        &a,
        WorkspaceCommand::BeginSubmission {
            id: chat.row.id.clone(),
            expected_revision: chat.revision,
            draft_revision: chat.draft_revision,
            text: "question".into(),
        },
    )
    .unwrap()
    {
        WorkspaceReply::Submission { receipt } => receipt,
        _ => panic!(),
    };
    assert!(request(
        &b,
        WorkspaceCommand::BeginSubmission {
            id: chat.row.id.clone(),
            expected_revision: chat.revision,
            draft_revision: chat.draft_revision,
            text: "duplicate".into()
        }
    )
    .is_err());
    request(
        &b,
        WorkspaceCommand::SetDraft {
            id: chat.row.id.clone(),
            expected_revision: 0,
            text: "next draft from another window".into(),
        },
    )
    .unwrap();
    request(
        &a,
        WorkspaceCommand::CompleteSubmission {
            receipt: receipt.clone(),
            text: "answer".into(),
        },
    )
    .unwrap();
    assert!(request(
        &a,
        WorkspaceCommand::CompleteSubmission {
            receipt,
            text: "late duplicate".into()
        }
    )
    .is_err());
    let loaded = match request(&b, WorkspaceCommand::LoadChat { id: chat.row.id }).unwrap() {
        WorkspaceReply::Chat { chat } => chat,
        _ => panic!(),
    };
    let snapshot = jterm_core::ai::ConversationSnapshot::from_json(&loaded.snapshot_json).unwrap();
    assert_eq!(snapshot.turns().len(), 2);
    assert_eq!(
        snapshot.active_chat().unwrap().draft(),
        "next draft from another window"
    );
}

#[test]
fn restarted_host_cannot_duplicate_or_stop_a_surviving_runtime() {
    let mut h = Harness::new();
    // Deliberately survive PTY hangup, so the old inherited lease remains
    // authoritative after SIGKILL of its supervising host.
    std::fs::write(h.root.join("bin/codex"), b"#!/bin/sh\ntrap '' HUP TERM\nprintf 'boot %s %s\\n' \"$$\" \"$$\" >> \"$JTERM_TEST_TRACE\"\nprintf 'READY %s\\n' \"$$\"\nwhile :; do sleep 1; done\n").unwrap();
    let mut first = h.attach();
    first.wait_for("READY");
    let original = h.runtime();
    h.host.kill().unwrap();
    h.host.wait().unwrap();
    first.wait_exit();
    assert!(process_live(original.pid));

    h.host = Command::new(HOST)
        .arg("--jterm-session-host")
        .env("JTERM_WORKSPACE_STATE_DIR", &h.state)
        .env("JTERM_TEST_TRACE", &h.trace)
        .env(
            "PATH",
            format!(
                "{}:{}",
                h.root.join("bin").display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            File::create(h.root.join("restarted-host-errors")).unwrap(),
        ))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Ok(WorkspaceReply::Catalog { rows }) =
            request(&h.client(), WorkspaceCommand::Refresh)
        {
            if rows
                .iter()
                .any(|row| row.id == original.id && row.runtime == RuntimeState::Conflict)
            {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "surviving runtime was not reported as a conflict"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut duplicate = h.attach();
    assert_ne!(duplicate.wait_exit(), 0);
    assert_eq!(
        std::fs::read_to_string(&h.trace).unwrap().lines().count(),
        1
    );
    assert!(request(
        &h.client(),
        WorkspaceCommand::Stop {
            id: original.id.clone()
        }
    )
    .is_err());
    assert!(
        process_live(original.pid),
        "new host signalled a process it does not supervise"
    );
    // Kill only our still-identical synthetic provider group.
    let args = std::fs::read(format!("/proc/{}/cmdline", original.pid)).unwrap();
    assert!(String::from_utf8_lossy(&args).contains(h.root.to_string_lossy().as_ref()));
    unsafe {
        libc::kill(-(original.pid as i32), libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_live(original.pid) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn native_profile_owner_blocks_cli_handoff_until_guard_releases() {
    let h = Harness::new();
    let home = h.session.provider_home().unwrap();
    std::fs::write(home.join("jterm-native-profile.json"), b"{}").unwrap();
    let guard =
        NativeSessionLease::acquire_at(&h.state, SessionProvider::Codex, &home, "native-profile")
            .unwrap();
    let mut denied = h.attach();
    denied.wait_for("live input owner");
    assert_ne!(denied.wait_exit(), 0);
    assert!(!h.trace.exists());
    drop(guard);
    let mut allowed = h.attach();
    allowed.wait_for("READY");
    request(
        &h.client(),
        WorkspaceCommand::Stop {
            id: session_id(&h.session).unwrap(),
        },
    )
    .unwrap();
    allowed.wait_exit();
}
