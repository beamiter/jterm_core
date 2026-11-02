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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const HOST: &str = env!("CARGO_BIN_EXE_jterm-sessiond");
fn host_executable() -> PathBuf {
    HOST.into()
}

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
fn native_profile_owner_excludes_cli_and_requires_explicit_access_grant_after_stop() {
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
    let mut explicit = h.attach();
    explicit.wait_for("fresh access grant");
    assert_ne!(explicit.wait_exit(), 0);
    assert!(
        !h.trace.exists(),
        "an unauthenticated legacy CLI was started"
    );
}

struct MockApi {
    url: String,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl MockApi {
    fn new(answer: String) -> Self {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (count, closed, opened) = (requests.clone(), stop.clone(), gate.clone());
        let worker = std::thread::spawn(move || {
            let mut children = Vec::new();
            while !closed.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let (count, closed, opened, answer) = (
                            count.clone(),
                            closed.clone(),
                            opened.clone(),
                            answer.clone(),
                        );
                        children.push(std::thread::spawn(move||{
                            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                            stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
                            let mut request=Vec::new();
                            let mut chunk=[0u8;4096];
                            loop {
                                let Ok(n)=stream.read(&mut chunk) else {return;};
                                if n==0 {return;}
                                request.extend_from_slice(&chunk[..n]);
                                if let Some(boundary)=request.windows(4).position(|w|w==b"\r\n\r\n") {
                                    let header=String::from_utf8_lossy(&request[..boundary]).to_lowercase();
                                    let length=header.lines().find_map(|line|line.strip_prefix("content-length:").and_then(|s|s.trim().parse::<usize>().ok())).unwrap_or(0);
                                    if request.len()>=boundary+4+length {break;}
                                }
                                assert!(request.len()<2*1024*1024);
                            }
                            count.fetch_add(1,Ordering::SeqCst);
                            let mut open=opened.0.lock().unwrap();
                            while !*open && !closed.load(Ordering::SeqCst) {open=opened.1.wait_timeout(open,Duration::from_millis(20)).unwrap().0;}
                            drop(open);
                            let body=serde_json::json!({"message":{"role":"assistant","content":answer},"done":true}).to_string();
                            let response=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body);
                            let _=stream.write_all(response.as_bytes());
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("local mock accept failed: {e}"),
                }
            }
            for child in children {
                child.join().unwrap();
            }
        });
        Self {
            url,
            requests,
            stop,
            gate,
            worker: Some(worker),
        }
    }
    fn wait_request(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.requests.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "local mock received no request");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn release(&self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }
}
impl Drop for MockApi {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.release();
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
struct ApiFrontend {
    child: Child,
    record: serde_json::Value,
}
impl ApiFrontend {
    fn new(h: &Harness, url: &str) -> Self {
        let config = h.root.join("api-frontend-config.json");
        let output = h.root.join("api-frontend-record.json");
        std::fs::write(
            &config,
            serde_json::json!({"state":h.state,"url":url,"output":output}).to_string(),
        )
        .unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "api_frontend_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("JTERM_TEST_API_FRONTEND", &config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let record = loop {
            if let Ok(bytes) = std::fs::read(&output) {
                if let Ok(record) = serde_json::from_slice(&bytes) {
                    break record;
                }
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "frontend fixture ended before admission"
            );
            assert!(
                Instant::now() < deadline,
                "frontend fixture was not admitted"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        Self { child, record }
    }
    fn receipt(&self) -> SubmissionReceipt {
        serde_json::from_value(self.record["receipt"].clone()).unwrap()
    }
    fn control(&self) -> ApiControlLease {
        serde_json::from_value(self.record["control"].clone()).unwrap()
    }
    fn exit(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for ApiFrontend {
    fn drop(&mut self) {
        self.exit();
    }
}

#[test]
#[ignore = "internal RPC fixture invoked by API lifecycle tests"]
fn api_frontend_fixture() {
    let path = std::env::var_os("JTERM_TEST_API_FRONTEND").expect("fixture configuration required");
    let config: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let client = WorkspaceClient::for_state_dir(config["state"].as_str().unwrap());
    let receipt = begin_api(&client);
    let launch = api_launch(receipt.clone(), config["url"].as_str().unwrap());
    let WorkspaceReply::Api {
        control: Some(control),
        ..
    } = request(
        &client,
        WorkspaceCommand::StartApi {
            launch: Box::new(launch),
        },
    )
    .unwrap()
    else {
        panic!("controller required")
    };
    std::fs::write(
        config["output"].as_str().unwrap(),
        serde_json::json!({"receipt":receipt,"control":control}).to_string(),
    )
    .unwrap();
    loop {
        std::thread::sleep(Duration::from_millis(100));
    }
}
fn begin_api(client: &WorkspaceClient) -> SubmissionReceipt {
    let WorkspaceReply::Chat { chat } = request(
        client,
        WorkspaceCommand::CreateChat {
            title: "Detached API fixture".into(),
        },
    )
    .unwrap() else {
        panic!("chat required")
    };
    let WorkspaceReply::Chat { chat } = request(
        client,
        WorkspaceCommand::SetDraft {
            id: chat.row.id.clone(),
            expected_revision: chat.draft_revision,
            text: "persist after GUI exit".into(),
        },
    )
    .unwrap() else {
        panic!("chat required")
    };
    let WorkspaceReply::Submission { receipt } = request(
        client,
        WorkspaceCommand::BeginSubmission {
            id: chat.row.id,
            expected_revision: chat.revision,
            draft_revision: chat.draft_revision,
            text: "persist after GUI exit".into(),
        },
    )
    .unwrap() else {
        panic!("receipt required")
    };
    receipt
}
fn api_launch(receipt: SubmissionReceipt, url: &str) -> ApiLaunch {
    let client = jterm_core::ai::AiClient::new(
        jterm_core::ai::Provider::Ollama,
        None,
        "fixture",
        url,
        64,
        None,
        false,
    )
    .unwrap();
    ApiLaunch::new(
        receipt,
        &client,
        None,
        &[jterm_core::ai::Turn {
            role: jterm_core::ai::Role::User,
            text: "persist after GUI exit".into(),
        }],
        false,
    )
    .unwrap()
}
fn api_snapshot(h: &Harness, id: &ConversationId) -> ApiSnapshot {
    let WorkspaceReply::Api { snapshot, .. } =
        request(&h.client(), WorkspaceCommand::InspectApi { id: id.clone() }).unwrap()
    else {
        panic!("API snapshot required")
    };
    snapshot
}
fn wait_api(h: &Harness, id: &ConversationId, state: ApiState) -> ApiSnapshot {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let snapshot = api_snapshot(h, id);
        if snapshot.state == state {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "expected {state:?}, got {snapshot:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
#[test]
fn api_survives_frontend_exit_hands_control_over_and_keeps_full_history() {
    let h = Harness::new();
    let answer = "durable answer ".repeat(10_000);
    let server = MockApi::new(answer.clone());
    let mut frontend = ApiFrontend::new(&h, &server.url);
    let receipt = frontend.receipt();
    let old_control = frontend.control();
    server.wait_request();
    assert!(request(
        &h.client(),
        WorkspaceCommand::AttachApi {
            id: receipt.id.clone(),
            epoch: receipt.epoch,
            view: "second-view".into()
        }
    )
    .is_err());
    assert!(request(
        &h.client(),
        WorkspaceCommand::CancelApi {
            control: old_control.clone()
        }
    )
    .is_err());
    request(
        &h.client(),
        WorkspaceCommand::StartApi {
            launch: Box::new(api_launch(receipt.clone(), &server.url)),
        },
    )
    .unwrap();
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    frontend.exit();
    let WorkspaceReply::Api {
        control: Some(control),
        ..
    } = request(
        &h.client(),
        WorkspaceCommand::AttachApi {
            id: receipt.id.clone(),
            epoch: receipt.epoch,
            view: "second-view".into(),
        },
    )
    .unwrap()
    else {
        panic!("new control required")
    };
    assert_ne!(control.generation, old_control.generation);
    assert!(request(
        &h.client(),
        WorkspaceCommand::CancelApi {
            control: old_control
        }
    )
    .is_err());
    let WorkspaceReply::Chat { chat } = request(
        &h.client(),
        WorkspaceCommand::LoadChat {
            id: receipt.id.clone(),
        },
    )
    .unwrap() else {
        panic!("chat required")
    };
    request(
        &h.client(),
        WorkspaceCommand::SetDraft {
            id: receipt.id.clone(),
            expected_revision: chat.draft_revision,
            text: "a newer independent draft".into(),
        },
    )
    .unwrap();
    server.release();
    assert_eq!(
        wait_api(&h, &receipt.id, ApiState::Completed)
            .result
            .as_deref(),
        Some(answer.as_str())
    );
    request(
        &h.client(),
        WorkspaceCommand::CompleteSubmission {
            receipt: receipt.clone(),
            text: "late GUI text must not replace the durable answer".into(),
        },
    )
    .unwrap();
    let WorkspaceReply::Messages { messages, .. } = request(
        &h.client(),
        WorkspaceCommand::History {
            id: receipt.id.clone(),
            before: None,
            limit: 2,
        },
    )
    .unwrap() else {
        panic!("history required")
    };
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].text, answer);
    let WorkspaceReply::Chat { chat } = request(
        &h.client(),
        WorkspaceCommand::LoadChat {
            id: receipt.id.clone(),
        },
    )
    .unwrap() else {
        panic!("chat required")
    };
    assert!(chat.snapshot_json.contains("a newer independent draft"));
    assert!(chat.snapshot_json.contains(&answer));
    assert!(!chat.has_older, "the complete pair fits in the active view");
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn api_detach_invalidates_control_and_explicit_cancel_restores_draft() {
    let h = Harness::new();
    let server = MockApi::new("must not commit after cancellation".into());
    let mut frontend = ApiFrontend::new(&h, &server.url);
    let receipt = frontend.receipt();
    server.wait_request();
    frontend.exit();
    let WorkspaceReply::Api {
        control: Some(first),
        ..
    } = request(
        &h.client(),
        WorkspaceCommand::AttachApi {
            id: receipt.id.clone(),
            epoch: receipt.epoch,
            view: "first-view".into(),
        },
    )
    .unwrap()
    else {
        panic!("control required")
    };
    request(
        &h.client(),
        WorkspaceCommand::DetachApi {
            control: first.clone(),
        },
    )
    .unwrap();
    let WorkspaceReply::Api {
        control: Some(second),
        ..
    } = request(
        &h.client(),
        WorkspaceCommand::AttachApi {
            id: receipt.id.clone(),
            epoch: receipt.epoch,
            view: "replacement-view".into(),
        },
    )
    .unwrap()
    else {
        panic!("replacement control required")
    };
    assert_ne!(first.generation, second.generation);
    assert!(request(&h.client(), WorkspaceCommand::CancelApi { control: first }).is_err());
    request(&h.client(), WorkspaceCommand::CancelApi { control: second }).unwrap();
    let stopped = wait_api(&h, &receipt.id, ApiState::Cancelled);
    assert!(stopped.result.is_none());
    let WorkspaceReply::Chat { chat } = request(
        &h.client(),
        WorkspaceCommand::LoadChat {
            id: receipt.id.clone(),
        },
    )
    .unwrap() else {
        panic!("chat required")
    };
    let snapshot = jterm_core::ai::ConversationSnapshot::from_json(&chat.snapshot_json).unwrap();
    let active = snapshot.active_chat().unwrap();
    assert_eq!(active.draft(), "persist after GUI exit");
    assert!(active.turns().is_empty());
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn api_host_restart_marks_unknown_outcome_without_resubmitting() {
    let mut h = Harness::new();
    let server = MockApi::new("late unverified answer".into());
    let mut frontend = ApiFrontend::new(&h, &server.url);
    let receipt = frontend.receipt();
    server.wait_request();
    wait_api(&h, &receipt.id, ApiState::Running);
    h.host.kill().unwrap();
    h.host.wait().unwrap();
    frontend.exit();
    h.host = Command::new(HOST)
        .arg("--jterm-session-host")
        .env("JTERM_WORKSPACE_STATE_DIR", &h.state)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while request(&h.client(), WorkspaceCommand::Refresh).is_err() {
        assert!(Instant::now() < deadline, "replacement host did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let unknown = wait_api(&h, &receipt.id, ApiState::Interrupted);
    assert!(unknown
        .error
        .as_deref()
        .unwrap()
        .contains("will not be submitted again"));
    let WorkspaceReply::Api { snapshot, control } = request(
        &h.client(),
        WorkspaceCommand::StartApi {
            launch: Box::new(api_launch(receipt.clone(), &server.url)),
        },
    )
    .unwrap() else {
        panic!("persisted execution required")
    };
    assert_eq!(snapshot.state, ApiState::Interrupted);
    assert!(control.is_none());
    assert!(request(
        &h.client(),
        WorkspaceCommand::CancelApi {
            control: frontend.control()
        }
    )
    .is_err());
    server.release();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    assert_eq!(api_snapshot(&h, &receipt.id).state, ApiState::Interrupted);
}

#[test]
fn dropping_api_observer_detaches_without_stopping_or_resubmitting() {
    let h = Harness::new();
    let server = MockApi::new("host retained the detached request".into());
    let mut frontend = ApiFrontend::new(&h, &server.url);
    let receipt = frontend.receipt();
    server.wait_request();
    frontend.exit();
    let observation = ApiObservation::default();
    let token = observation.token();
    let state = h.state.clone();
    let id = receipt.id.clone();
    let epoch = receipt.epoch;
    request(
        &h.client(),
        WorkspaceCommand::AttachApi {
            id: receipt.id.clone(),
            epoch,
            view: "drop-view".into(),
        },
    )
    .unwrap();
    let (ready, watching) = std::sync::mpsc::sync_channel(1);
    let observer = std::thread::spawn(move || {
        HostedApiClient::for_state_dir(state).observe(
            id,
            epoch,
            "drop-view".into(),
            &token,
            &mut |_| {
                let _ = ready.try_send(());
            },
        )
    });
    watching
        .recv_timeout(Duration::from_secs(5))
        .expect("observer did not receive its snapshot");
    drop(observation);
    assert!(observer.join().unwrap().unwrap_err().contains("detached"));
    let WorkspaceReply::Api {
        snapshot,
        control: Some(_),
        ..
    } = request(
        &h.client(),
        WorkspaceCommand::AttachApi {
            id: receipt.id.clone(),
            epoch,
            view: "replacement-view".into(),
        },
    )
    .unwrap()
    else {
        panic!("detached control was not released")
    };
    assert!(snapshot.state.is_live());
    let WorkspaceReply::Chat { chat } = request(
        &h.client(),
        WorkspaceCommand::LoadChat {
            id: receipt.id.clone(),
        },
    )
    .unwrap() else {
        panic!("chat required")
    };
    assert_eq!(chat.api_job.unwrap().user_text, "persist after GUI exit");
    server.release();
    wait_api(&h, &receipt.id, ApiState::Completed);
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn api_launch_credentials_are_not_debugged_or_persisted() {
    let h = Harness::new();
    let server = MockApi::new("credential fixture completed".into());
    let receipt = begin_api(&h.client());
    let key = "synthetic-key-kept-only-in-memory-836974";
    let client = jterm_core::ai::AiClient::new(
        jterm_core::ai::Provider::Ollama,
        Some(key.into()),
        "fixture",
        &server.url,
        64,
        None,
        true,
    )
    .unwrap();
    let launch = ApiLaunch::new(
        receipt.clone(),
        &client,
        Some("synthetic private system prompt".into()),
        &[jterm_core::ai::Turn {
            role: jterm_core::ai::Role::User,
            text: "synthetic private prompt".into(),
        }],
        false,
    )
    .unwrap();
    let debug = format!("{launch:?}");
    assert!(!debug.contains(key));
    assert!(!debug.contains("synthetic private"));
    request(
        &h.client(),
        WorkspaceCommand::StartApi {
            launch: Box::new(launch),
        },
    )
    .unwrap();
    server.wait_request();
    server.release();
    wait_api(&h, &receipt.id, ApiState::Completed);
    for entry in std::fs::read_dir(&h.state).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            let bytes = std::fs::read(entry.path()).unwrap();
            assert!(
                !bytes
                    .windows(key.len())
                    .any(|window| window == key.as_bytes()),
                "launch credential persisted to {:?}",
                entry.file_name()
            );
        }
    }
    let WorkspaceReply::Api { snapshot, .. } =
        request(&h.client(), WorkspaceCommand::ResolveApi { receipt }).unwrap()
    else {
        panic!("durable receipt must resolve")
    };
    assert_eq!(snapshot.state, ApiState::Completed);
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn resolving_an_unadmitted_api_receipt_restores_only_its_pending_draft() {
    let h = Harness::new();
    let receipt = begin_api(&h.client());
    let WorkspaceReply::Chat { chat } = request(
        &h.client(),
        WorkspaceCommand::ResolveApi {
            receipt: receipt.clone(),
        },
    )
    .unwrap() else {
        panic!("unadmitted request should be rolled back")
    };
    assert!(chat.api_job.is_none());
    let snapshot = jterm_core::ai::ConversationSnapshot::from_json(&chat.snapshot_json).unwrap();
    assert_eq!(
        snapshot.active_chat().unwrap().draft(),
        "persist after GUI exit"
    );
    assert!(
        request(&h.client(), WorkspaceCommand::ResolveApi { receipt }).is_err(),
        "a resolved receipt must never acquire new authority"
    );
}

#[test]
fn restored_api_view_hides_only_the_acknowledged_draft_version() {
    let h = Harness::new();
    let server = MockApi::new("versioned draft completed".into());
    let receipt = begin_api(&h.client());
    request(
        &h.client(),
        WorkspaceCommand::StartApi {
            launch: Box::new(api_launch(receipt.clone(), &server.url)),
        },
    )
    .unwrap();
    server.wait_request();
    let WorkspaceReply::Chat { chat } = request(
        &h.client(),
        WorkspaceCommand::LoadChat {
            id: receipt.id.clone(),
        },
    )
    .unwrap() else {
        panic!("chat required")
    };
    let snapshot = jterm_core::ai::ConversationSnapshot::from_json(&chat.snapshot_json).unwrap();
    assert_eq!(
        snapshot.active_chat().unwrap().draft(),
        "",
        "reopened view must not duplicate the submitted message in its composer"
    );
    let WorkspaceReply::Chat { chat } = request(
        &h.client(),
        WorkspaceCommand::SetDraft {
            id: receipt.id.clone(),
            expected_revision: chat.draft_revision,
            text: "persist after GUI exit".into(),
        },
    )
    .unwrap() else {
        panic!("new draft required")
    };
    let snapshot = jterm_core::ai::ConversationSnapshot::from_json(&chat.snapshot_json).unwrap();
    assert_eq!(
        snapshot.active_chat().unwrap().draft(),
        "persist after GUI exit",
        "equal text with a newer revision is an independent draft"
    );
    server.release();
    wait_api(&h, &receipt.id, ApiState::Completed);
    let WorkspaceReply::Chat { chat } =
        request(&h.client(), WorkspaceCommand::LoadChat { id: receipt.id }).unwrap()
    else {
        panic!("chat required")
    };
    let snapshot = jterm_core::ai::ConversationSnapshot::from_json(&chat.snapshot_json).unwrap();
    assert_eq!(
        snapshot.active_chat().unwrap().draft(),
        "persist after GUI exit"
    );
}

#[test]
fn api_worker_starts_after_the_running_hosts_executable_was_unlinked() {
    let mut h = Harness::new();
    h.host.kill().unwrap();
    h.host.wait().unwrap();
    let executable = h.root.join("bin/private-session-host");
    std::fs::copy(HOST, &executable).unwrap();
    h.host = Command::new(&executable)
        .arg("--jterm-session-host")
        .env("JTERM_WORKSPACE_STATE_DIR", &h.state)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while request(&h.client(), WorkspaceCommand::Refresh).is_err() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::remove_file(&executable).unwrap();
    let server = MockApi::new("original running image survived installation replacement".into());
    let receipt = begin_api(&h.client());
    request(
        &h.client(),
        WorkspaceCommand::StartApi {
            launch: Box::new(api_launch(receipt.clone(), &server.url)),
        },
    )
    .unwrap();
    server.wait_request();
    server.release();
    wait_api(&h, &receipt.id, ApiState::Completed);
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

fn native_fixture(h: &Harness) -> jterm_core::agent_workspace::NativeLaunch {
    use jterm_core::agent_task::*;
    let repository = h.root.join("native-repo");
    std::fs::create_dir(&repository).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("/usr/bin/git")
            .current_dir(&repository)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet"]);
    std::fs::write(repository.join("tracked"), "fixture").unwrap();
    git(&["add", "tracked"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "fixture",
    ]);
    let managed = WorktreeService::new(h.root.join("native-managed"))
        .unwrap()
        .create(&CreateWorktreeRequest::new(
            &repository,
            "hosted",
            "app/hosted-fixture",
            "HEAD",
        ))
        .unwrap();
    let mut tasks = TaskManager::new();
    let id = tasks
        .create(NewTask {
            title: "Host owned Claude".into(),
            provider: AgentProvider::Claude,
            repo_root: managed.repository,
            worktree_path: managed.path,
            branch: managed.branch,
            base_commit: managed.head,
            source_context: Some(SemanticCommandContext {
                source_session_id: "fixture-source".into(),
                source_execution_id: "fixture-execution".into(),
                source_sequence: 1,
                source_shell: Some("/bin/sh".into()),
                command: Some("false".into()),
                command_exact: true,
                command_truncated: false,
                cwd: Some(repository.to_string_lossy().into_owned()),
                cwd_after: Some(repository.to_string_lossy().into_owned()),
                exit_code: Some(1),
                duration_ms: Some(1),
                output_text: "fixture failure".into(),
                output_available: true,
                output_truncated: false,
                output_total_bytes: 15,
                started_at: None,
                finished_at: None,
            }),
        })
        .unwrap();
    let profile = h.root.join("claude-profile");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&profile)
        .unwrap();
    let fake = h.root.join("bin/claude");
    std::fs::write(&fake,r##"#!/usr/bin/python3
import sys,os,pathlib,json,time
args=sys.argv[1:];root=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR']);mode='--resume' if '--resume' in args else '--session-id';ident=args[args.index(mode)+1]
if '-p' not in args:
 print('EXACT_CLI:'+ident,flush=True)
 for line in sys.stdin: print('CLI_REPLY:'+line.rstrip(),flush=True)
 sys.exit(0)
(root/'native-provider.pid').write_text(str(os.getpid()))
trace=root/'native-invocations';calls=trace.read_text().splitlines() if trace.exists() else [];number=len(calls)+1
with trace.open('a') as f:f.write(mode+'|'+ident+'\n')
history=root/'projects'/'fixture';history.mkdir(parents=True,exist_ok=True)
(history/(ident+'.jsonl')).write_text(json.dumps({'sessionId':ident,'cwd':os.getcwd(),'type':'user','message':{'role':'user','content':'fixture'}})+'\n')
print(json.dumps({'type':'system','subtype':'init','session_id':ident}),flush=True)
print(json.dumps({'type':'assistant','message':{'content':[{'type':'text','text':'Preview '+str(number)}]}}),flush=True)
deadline=time.monotonic()+60
while not (root/('release-'+str(number))).exists() and time.monotonic()<deadline:time.sleep(.02)
print(json.dumps({'type':'result','subtype':'success','is_error':False,'result':'Reviewed '+str(number)}),flush=True)
"##).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    NativeLaunch {namespace:"fixture:native".into(),task:tasks.get(id).unwrap().clone(),view:"frontend-a".into(),policy:NativePromptPolicy {share_command_context:true,redact_secrets:true},context:serde_json::from_value(serde_json::json!({"search_path":format!("{}:/usr/bin:/bin",h.root.join("bin").display()),"codex_home":h.root.join("unused-codex"),"claude_home":profile,"environment":[]})).unwrap()}
}
fn inspect_native(client: &WorkspaceClient, id: &ConversationId) -> NativeSnapshot {
    match request(client, WorkspaceCommand::InspectNative { id: id.clone() }).unwrap() {
        WorkspaceReply::Native { snapshot, .. } => *snapshot,
        other => panic!("Unexpected reply: {other:?}"),
    }
}
fn wait_native(
    client: &WorkspaceClient,
    id: &ConversationId,
    predicate: impl Fn(&NativeSnapshot) -> bool,
) -> NativeSnapshot {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let result = request(client, WorkspaceCommand::InspectNative { id: id.clone() });
        if let Ok(WorkspaceReply::Native { snapshot, .. }) = &result {
            if predicate(snapshot) {
                return snapshot.as_ref().clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "Native condition timed out: {result:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
#[test]
#[ignore]
fn native_frontend_fixture() {
    let Ok(state) = std::env::var("JTERM_NATIVE_FIXTURE_STATE") else {
        return;
    };
    let launch: NativeLaunch =
        serde_json::from_str(&std::env::var("JTERM_NATIVE_FIXTURE_LAUNCH").unwrap()).unwrap();
    let client = WorkspaceClient::for_state_dir(&state);
    let reply = request(
        &client,
        WorkspaceCommand::StartNative {
            launch: Box::new(launch),
        },
    )
    .unwrap();
    std::fs::write(
        std::env::var("JTERM_NATIVE_FIXTURE_REPLY").unwrap(),
        serde_json::to_vec(&reply).unwrap(),
    )
    .unwrap();
    loop {
        std::thread::sleep(Duration::from_millis(100));
    }
}
#[test]
fn native_gui_exit_reconnect_exact_turn_and_single_writer_handoff() {
    let h = Harness::new();
    let launch = native_fixture(&h);
    let profile = launch.context.claude_home.clone();
    let id = task_id(&launch.namespace, &launch.task.id.to_string()).unwrap();
    let file = h.root.join("native-front-reply");
    let mut frontend = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "native_frontend_fixture",
            "--nocapture",
        ])
        .env("JTERM_NATIVE_FIXTURE_STATE", &h.state)
        .env(
            "JTERM_NATIVE_FIXTURE_LAUNCH",
            serde_json::to_string(&launch).unwrap(),
        )
        .env("JTERM_NATIVE_FIXTURE_REPLY", &file)
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            File::create(h.root.join("native-front-error")).unwrap(),
        ))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !file.exists() {
        assert!(
            Instant::now() < deadline,
            "frontend did not admit: {}",
            std::fs::read_to_string(h.root.join("native-front-error")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let old: WorkspaceReply = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let old_control = match old {
        WorkspaceReply::Native {
            control: Some(control),
            ..
        } => control,
        other => panic!("{other:?}"),
    };
    let client = h.client();
    let running = wait_native(&client, &id, |s| {
        s.view
            .as_ref()
            .is_some_and(|v| v.agent_text.contains("Preview 1"))
    });
    assert!(running.ownership_locked);
    frontend.kill().unwrap();
    frontend.wait().unwrap();
    let still = inspect_native(&client, &id);
    assert!(still.live, "GUI exit stopped the native worker");
    let control = match request(
        &client,
        WorkspaceCommand::AttachNative {
            id: id.clone(),
            incarnation: still.incarnation.clone(),
            view: "frontend-b".into(),
        },
    )
    .unwrap()
    {
        WorkspaceReply::Native {
            control: Some(control),
            ..
        } => control,
        other => panic!("{other:?}"),
    };
    assert!(request(
        &client,
        WorkspaceCommand::ControlNative {
            control: old_control,
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::Cancel)
        }
    )
    .is_err());
    std::fs::write(profile.join("release-1"), "").unwrap();
    let ready = wait_native(&client, &id, |s| s.can_prompt);
    assert_eq!(
        ready.task.status,
        jterm_core::agent_task::TaskStatus::ReadyForReview
    );
    let thread = ready.view.unwrap().provider_thread_id.unwrap();
    let operation = uuid::Uuid::new_v4().to_string();
    let command = WorkspaceCommand::ControlNative {
        control: control.clone(),
        operation: operation.clone(),
        action: Box::new(NativeAction::Prompt {
            text: "second synthetic turn".into(),
            policy: launch.policy,
            receipt: None,
        }),
    };
    request(&client, command.clone()).unwrap();
    request(&client, command).unwrap();
    wait_native(&client, &id, |s| {
        s.view
            .as_ref()
            .is_some_and(|v| v.agent_text.contains("Preview 2"))
    });
    std::fs::write(profile.join("release-2"), "").unwrap();
    wait_native(&client, &id, |s| {
        s.can_prompt && s.view.as_ref().is_some_and(|v| v.completed_turns == 2)
    });
    let calls = std::fs::read_to_string(profile.join("native-invocations")).unwrap();
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        vec![
            format!("--session-id|{thread}"),
            format!("--resume|{thread}")
        ]
    );
    request(
        &client,
        WorkspaceCommand::ControlNative {
            control: control.clone(),
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::Finish),
        },
    )
    .unwrap();
    wait_native(&client, &id, |s| !s.ownership_locked);
    request(
        &client,
        WorkspaceCommand::ControlNative {
            control: control.clone(),
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::Handoff {
                context: launch.context.clone(),
            }),
        },
    )
    .unwrap();
    let terminal = wait_native(&client, &id, |s| s.terminal.is_some());
    assert_eq!(terminal.terminal.unwrap().id, thread);
    assert!(terminal.ownership_locked);
    assert!(request(
        &client,
        WorkspaceCommand::ControlNative {
            control,
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::Prompt {
                text: "must not race CLI".into(),
                policy: launch.policy,
                receipt: None,
            })
        }
    )
    .is_err());
    assert_eq!(
        std::fs::read_to_string(profile.join("native-invocations"))
            .unwrap()
            .lines()
            .count(),
        2,
        "handoff submitted a new native goal"
    );
}

#[test]
fn native_host_crash_preserves_history_and_refuses_surviving_owner() {
    let mut h = Harness::new();
    let launch = native_fixture(&h);
    let profile = launch.context.claude_home.clone();
    let id = task_id(&launch.namespace, &launch.task.id.to_string()).unwrap();
    let client = h.client();
    request(
        &client,
        WorkspaceCommand::StartNative {
            launch: Box::new(launch.clone()),
        },
    )
    .unwrap();
    let running = wait_native(&client, &id, |s| {
        s.view
            .as_ref()
            .is_some_and(|v| v.agent_text.contains("Preview 1"))
    });
    let pid: u32 = std::fs::read_to_string(profile.join("native-provider.pid"))
        .unwrap()
        .parse()
        .unwrap();
    h.host.kill().unwrap();
    h.host.wait().unwrap();
    assert!(process_live(pid));
    h.host = Command::new(HOST)
        .arg("--jterm-session-host")
        .env("JTERM_WORKSPACE_STATE_DIR", &h.state)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            File::create(h.root.join("native-restarted-host-errors")).unwrap(),
        ))
        .spawn()
        .unwrap();
    let client = h.client();
    let recovered = wait_native(&client, &id, |s| s.interrupted);
    assert!(!recovered.live);
    assert!(!recovered.can_prompt);
    assert!(recovered
        .view
        .as_ref()
        .unwrap()
        .pending_approvals
        .is_empty());
    let attach = request(
        &client,
        WorkspaceCommand::AttachNative {
            id: id.clone(),
            incarnation: running.incarnation,
            view: "after-crash".into(),
        },
    );
    assert!(
        attach.is_err(),
        "surviving provider was admitted as a second writer: {attach:?}"
    );
    request(
        &client,
        WorkspaceCommand::StartNative {
            launch: Box::new(launch),
        },
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(profile.join("native-invocations"))
            .unwrap()
            .lines()
            .count(),
        1,
        "host restart repeated a model request"
    );
    let args = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    assert!(String::from_utf8_lossy(&args).contains(h.root.to_string_lossy().as_ref()));
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[test]
fn native_remote_view_drop_detaches_without_stopping_the_runtime() {
    use jterm_core::agent_task::{AgentRuntimeManager, TaskManager};
    let h = Harness::new();
    let launch = native_fixture(&h);
    let id = task_id(&launch.namespace, &launch.task.id.to_string()).unwrap();
    let client = h.client();
    let control = match request(
        &client,
        WorkspaceCommand::StartNative {
            launch: Box::new(launch.clone()),
        },
    )
    .unwrap()
    {
        WorkspaceReply::Native {
            control: Some(control),
            ..
        } => control,
        reply => panic!("{reply:?}"),
    };
    let running = wait_native(&client, &id, |s| {
        s.view
            .as_ref()
            .is_some_and(|v| v.agent_text.contains("Preview 1"))
    });
    request(&client, WorkspaceCommand::DetachNative { control }).unwrap();
    let mut view = AgentRuntimeManager::hosted_with_client("observer", h.client());
    let mut tasks = TaskManager::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    while view.snapshot(launch.task.id).is_none() {
        view.poll(&mut tasks, launch.policy);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    // A control action admits a fresh lease; policy is harmless to the worker.
    view.load_native_history(launch.task.id, false).unwrap();
    for _ in 0..10 {
        view.poll(&mut tasks, launch.policy);
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(view);
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let result = request(
            &client,
            WorkspaceCommand::AttachNative {
                id: id.clone(),
                incarnation: running.incarnation.clone(),
                view: "after-view-drop".into(),
            },
        );
        if matches!(
            result,
            Ok(WorkspaceReply::Native {
                control: Some(_),
                ..
            })
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "closed view retained its control lease: {result:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        inspect_native(&client, &id).live,
        "dropping only the remote view stopped the provider"
    );
}

/// Isolated Xvfb acceptance setup. The provider is our local Python fixture;
/// no actual provider history, credentials, network or desktop is involved.
#[test]
#[ignore]
fn native_gui_acceptance_fixture() {
    let Ok(output) = std::env::var("JTERM_GUI_NATIVE_FIXTURE") else {
        return;
    };
    let h = Harness::new();
    let launch = native_fixture(&h);
    let profile = launch.context.claude_home.clone();
    let id = task_id(&launch.namespace, &launch.task.id.to_string()).unwrap();
    let client = h.client();
    let control = match request(
        &client,
        WorkspaceCommand::StartNative {
            launch: Box::new(launch.clone()),
        },
    )
    .unwrap()
    {
        WorkspaceReply::Native {
            control: Some(control),
            ..
        } => control,
        reply => panic!("{reply:?}"),
    };
    std::fs::write(profile.join("release-1"), "").unwrap();
    let ready = wait_native(&client, &id, |s| s.can_prompt);
    request(&client, WorkspaceCommand::DetachNative { control }).unwrap();
    std::fs::write(&output,serde_json::to_vec_pretty(&serde_json::json!({"state_dir":h.state,"profile":profile,"task_id":launch.task.id,"conversation_id":id,"thread_id":ready.view.unwrap().provider_thread_id,"repo":launch.task.repo_root,"worktree":launch.task.worktree_path})).unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1800);
    let stop = PathBuf::from(format!("{output}.stop"));
    while Instant::now() < deadline && !stop.exists() {
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn native_redacted_source_evidence_never_enters_database_or_wal() {
    let h = Harness::new();
    let mut launch = native_fixture(&h);
    let secret = "synthetic-context-secret-123456";
    let context = launch.task.source_context.as_mut().unwrap();
    context.command = Some(format!("API_TOKEN={secret} false"));
    context.output_text = format!("API_TOKEN={secret}");
    let id = task_id(&launch.namespace, &launch.task.id.to_string()).unwrap();
    let client = h.client();
    request(
        &client,
        WorkspaceCommand::StartNative {
            launch: Box::new(launch),
        },
    )
    .unwrap();
    let snapshot = wait_native(&client, &id, |s| {
        s.view
            .as_ref()
            .is_some_and(|v| v.agent_text.contains("Preview 1"))
    });
    assert!(snapshot.validation_evidence_redacted);
    assert!(!snapshot.task.source_context.as_ref().unwrap().command_exact);
    for name in ["workspace.sqlite3", "workspace.sqlite3-wal"] {
        if let Ok(bytes) = std::fs::read(h.state.join(name)) {
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|part| part == secret.as_bytes()),
                "source secret was persisted to {name}"
            );
        }
    }
}
#[test]
fn native_prompt_receipt_reports_host_conflict_without_dispatching() {
    use jterm_core::agent_task::{AgentRuntimeManager, TaskManager};
    let h = Harness::new();
    let launch = native_fixture(&h);
    let profile = launch.context.claude_home.clone();
    let id = task_id(&launch.namespace, &launch.task.id.to_string()).unwrap();
    let client = h.client();
    request(
        &client,
        WorkspaceCommand::StartNative {
            launch: Box::new(launch.clone()),
        },
    )
    .unwrap();
    std::fs::write(profile.join("release-1"), "").unwrap();
    wait_native(&client, &id, |s| s.can_prompt);
    let mut view = AgentRuntimeManager::hosted_with_client("second-view", h.client());
    let mut tasks = TaskManager::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    while view.snapshot(launch.task.id).is_none() {
        view.poll(&mut tasks, launch.policy);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let receipt = uuid::Uuid::new_v4().to_string();
    view.prompt_native_tracked(
        &tasks,
        launch.task.id,
        "keep this rejected draft",
        launch.policy,
        receipt.clone(),
    )
    .unwrap();
    loop {
        view.poll(&mut tasks, launch.policy);
        if let Some(outcome) = view
            .take_prompt_outcomes()
            .into_iter()
            .find(|o| o.receipt == receipt)
        {
            assert!(outcome.result.is_err());
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_to_string(profile.join("native-invocations"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}
#[test]
fn native_delayed_attach_rechecks_current_sharing_policy() {
    use jterm_core::agent_task::{AgentRuntimeManager, NativePromptPolicy, TaskManager};
    let h = Harness::new();
    let launch = native_fixture(&h);
    let profile = launch.context.claude_home.clone();
    let id = task_id(&launch.namespace, &launch.task.id.to_string()).unwrap();
    let client = h.client();
    let control = match request(
        &client,
        WorkspaceCommand::StartNative {
            launch: Box::new(launch.clone()),
        },
    )
    .unwrap()
    {
        WorkspaceReply::Native {
            control: Some(control),
            ..
        } => control,
        reply => panic!("{reply:?}"),
    };
    std::fs::write(profile.join("release-1"), "").unwrap();
    wait_native(&client, &id, |s| s.can_prompt);
    request(&client, WorkspaceCommand::DetachNative { control }).unwrap();
    let mut view = AgentRuntimeManager::hosted_with_client("policy-view", h.client());
    let mut tasks = TaskManager::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    while view.snapshot(launch.task.id).is_none() {
        view.poll(&mut tasks, launch.policy);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let receipt = uuid::Uuid::new_v4().to_string();
    view.prompt_native_tracked(
        &tasks,
        launch.task.id,
        "must stay local",
        launch.policy,
        receipt.clone(),
    )
    .unwrap();
    let revoked = NativePromptPolicy {
        share_command_context: false,
        redact_secrets: true,
    };
    loop {
        view.poll(&mut tasks, revoked);
        if let Some(outcome) = view
            .take_prompt_outcomes()
            .into_iter()
            .find(|o| o.receipt == receipt)
        {
            assert!(outcome.result.is_err());
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_to_string(profile.join("native-invocations"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

fn stopped_native_for_validation(
    h: &Harness,
    command: &str,
) -> (NativeLaunch, ConversationId, NativeControlLease) {
    let mut launch = native_fixture(h);
    let id = task_id(&launch.namespace, &launch.task.id.to_string()).unwrap();
    launch.task.source_context.as_mut().unwrap().command = Some(command.into());
    let client = h.client();
    let control = match request(
        &client,
        WorkspaceCommand::StartNative {
            launch: Box::new(launch.clone()),
        },
    )
    .unwrap()
    {
        WorkspaceReply::Native {
            control: Some(control),
            ..
        } => control,
        reply => panic!("{reply:?}"),
    };
    std::fs::write(launch.context.claude_home.join("release-1"), "").unwrap();
    wait_native(&client, &id, |s| s.can_prompt);
    request(
        &client,
        WorkspaceCommand::ControlNative {
            control: control.clone(),
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::Finish),
        },
    )
    .unwrap();
    wait_native(&client, &id, |s| !s.ownership_locked);
    (launch, id, control)
}

#[test]
fn hosted_validation_detaches_replays_output_and_requires_controlled_acceptance() {
    use jterm_core::agent_task::{TaskStatus, TaskValidationStatus};
    let h = Harness::new();
    let gate = h.root.join("allow-validation-finish");
    let command=format!("printf validation-started; while ! test -f '{}'; do sleep 0.02; done; printf validation-finished",gate.display());
    let (_launch, id, control) = stopped_native_for_validation(&h, &command);
    let client = h.client();
    let snapshot = inspect_native(&client, &id);
    let operation = uuid::Uuid::new_v4().to_string();
    let start = WorkspaceCommand::ControlNative {
        control: control.clone(),
        operation,
        action: Box::new(NativeAction::Validate {
            expected_sequence: snapshot.sequence,
        }),
    };
    request(&client, start.clone()).unwrap();
    request(&client, start).unwrap();
    let running = wait_native(&client, &id, |s| {
        s.validation
            .as_ref()
            .is_some_and(|v| v.terminal.is_some() && v.output.contains("validation-started"))
    });
    assert!(running.ownership_locked);
    assert_eq!(
        running.task.validation.status,
        TaskValidationStatus::Running
    );
    let terminal = running.validation.unwrap().terminal.unwrap();
    let runtime = match request(
        &client,
        WorkspaceCommand::Runtime {
            id: terminal.id.clone(),
        },
    )
    .unwrap()
    {
        WorkspaceReply::Runtime { snapshot } => snapshot,
        reply => panic!("{reply:?}"),
    };
    let attach = |terminal: &OwnedTerminalDescriptor| {
        let mut command = Command::new(host_executable());
        command
            .args([
                "--jterm-owned-attach",
                &serde_json::to_string(terminal).unwrap(),
            ])
            .env("JTERM_WORKSPACE_STATE_DIR", &h.state)
            .current_dir(&terminal.cwd);
        Terminal::spawn(command)
    };
    let mut view = attach(&terminal);
    view.wait_for("validation-started");
    view.child.terminate_group().unwrap();
    view.wait_exit();
    let after = inspect_native(&client, &id);
    assert_eq!(after.task.validation.status, TaskValidationStatus::Running);
    let same = match request(
        &client,
        WorkspaceCommand::Runtime {
            id: terminal.id.clone(),
        },
    )
    .unwrap()
    {
        WorkspaceReply::Runtime { snapshot } => snapshot,
        _ => unreachable!(),
    };
    assert_eq!(same.pid, runtime.pid);
    assert!(request(
        &client,
        WorkspaceCommand::ControlNative {
            control: control.clone(),
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::MutateTask {
                expected_sequence: after.sequence,
                mutation: NativeTaskMutation::Complete
            })
        }
    )
    .is_err());
    std::fs::write(&gate, "").unwrap();
    let done = wait_native(&client, &id, |s| {
        s.task.validation.status == TaskValidationStatus::Passed && !s.ownership_locked
    });
    assert!(done
        .validation
        .as_ref()
        .unwrap()
        .output
        .contains("validation-finished"));
    let mut replay = attach(&terminal);
    replay.wait_for("validation-finished");
    replay.wait_for("Validation exited:");
    replay.input(b"\x04");
    assert_eq!(replay.wait_exit(), 0);
    let mut stale = terminal.clone();
    stale.epoch = uuid::Uuid::new_v4().to_string();
    let mut rejected = attach(&stale);
    rejected.wait_for("Stale owned runtime epoch");
    assert_ne!(rejected.wait_exit(), 0);
    let accepted = request(
        &client,
        WorkspaceCommand::ControlNative {
            control: control.clone(),
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::MutateTask {
                expected_sequence: done.sequence,
                mutation: NativeTaskMutation::Complete,
            }),
        },
    )
    .unwrap();
    let sequence = match accepted {
        WorkspaceReply::Native { snapshot, .. } => {
            assert_eq!(snapshot.task.status, TaskStatus::Completed);
            snapshot.sequence
        }
        _ => unreachable!(),
    };
    assert!(
        request(
            &client,
            WorkspaceCommand::ControlNative {
                control: control.clone(),
                operation: uuid::Uuid::new_v4().to_string(),
                action: Box::new(NativeAction::MutateTask {
                    expected_sequence: done.sequence,
                    mutation: NativeTaskMutation::Archive
                })
            }
        )
        .is_err(),
        "stale lifecycle CAS was accepted"
    );
    let current = inspect_native(&client, &id);
    assert!(current.sequence >= sequence);
    request(
        &client,
        WorkspaceCommand::ControlNative {
            control: control.clone(),
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::MutateTask {
                expected_sequence: current.sequence,
                mutation: NativeTaskMutation::Archive,
            }),
        },
    )
    .unwrap();
    let rows = match request(&client, WorkspaceCommand::Refresh).unwrap() {
        WorkspaceReply::Catalog { rows } => rows,
        _ => unreachable!(),
    };
    assert!(rows.iter().find(|row| row.id == id).unwrap().archived);
    let archived = inspect_native(&client, &id);
    request(
        &client,
        WorkspaceCommand::ControlNative {
            control,
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::MutateTask {
                expected_sequence: archived.sequence,
                mutation: NativeTaskMutation::Restore,
            }),
        },
    )
    .unwrap();
    assert_eq!(
        inspect_native(&client, &id).task.status,
        TaskStatus::Completed
    );
}

fn stopped_native_validation_fixture(
    command: String,
) -> (
    Harness,
    WorkspaceClient,
    ConversationId,
    NativeControlLease,
    NativeLaunch,
) {
    stopped_native_validation_fixture_with_redaction(command, true)
}
fn stopped_native_validation_fixture_with_redaction(
    command: String,
    redact_secrets: bool,
) -> (
    Harness,
    WorkspaceClient,
    ConversationId,
    NativeControlLease,
    NativeLaunch,
) {
    let h = Harness::new();
    let mut launch = native_fixture(&h);
    launch.policy.redact_secrets = redact_secrets;
    launch.task.source_context.as_mut().unwrap().command =
        Some(command.replace("FIXTURE_ROOT", &h.root.to_string_lossy()));
    let client = h.client();
    let id = task_id(&launch.namespace, &launch.task.id.to_string()).unwrap();
    let control = match request(
        &client,
        WorkspaceCommand::StartNative {
            launch: Box::new(launch.clone()),
        },
    )
    .unwrap()
    {
        WorkspaceReply::Native {
            control: Some(control),
            ..
        } => control,
        reply => panic!("{reply:?}"),
    };
    std::fs::write(launch.context.claude_home.join("release-1"), "").unwrap();
    wait_native(&client, &id, |s| s.can_prompt);
    request(
        &client,
        WorkspaceCommand::ControlNative {
            control: control.clone(),
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::Finish),
        },
    )
    .unwrap();
    wait_native(&client, &id, |s| !s.ownership_locked);
    (h, client, id, control, launch)
}
fn attach_owned_fixture(h: &Harness, terminal: &OwnedTerminalDescriptor) -> Terminal {
    let mut command = Command::new(host_executable());
    command
        .args([
            "--jterm-owned-attach",
            &serde_json::to_string(terminal).unwrap(),
        ])
        .current_dir(&h.root)
        .env("JTERM_WORKSPACE_STATE_DIR", &h.state);
    Terminal::spawn(command)
}
#[test]
fn hosted_validation_detaches_reattaches_once_and_commits_controlled_task_mutations() {
    use jterm_core::agent_task::{TaskStatus, TaskValidationStatus};
    let(h,client,id,control,_) = stopped_native_validation_fixture("printf 'run\\n' >> FIXTURE_ROOT/validation-count; printf validation-started; while ! test -f FIXTURE_ROOT/validation-gate; do sleep .02; done; printf validation-final".into());
    let before = inspect_native(&client, &id);
    let operation = uuid::Uuid::new_v4().to_string();
    let start = WorkspaceCommand::ControlNative {
        control: control.clone(),
        operation: operation.clone(),
        action: Box::new(NativeAction::Validate {
            expected_sequence: before.sequence,
        }),
    };
    request(&client, start.clone()).unwrap();
    let running = wait_native(&client, &id, |s| {
        s.validation
            .as_ref()
            .is_some_and(|v| v.terminal.is_some() && v.output.contains("validation-started"))
    });
    let terminal = running.validation.unwrap().terminal.unwrap();
    let mut view = attach_owned_fixture(&h, &terminal);
    view.wait_for("validation-started");
    view.child.terminate_group().unwrap();
    view.wait_exit();
    request(&client, WorkspaceCommand::DetachNative { control }).unwrap();
    let current = inspect_native(&client, &id);
    let control = match request(
        &client,
        WorkspaceCommand::AttachNative {
            id: id.clone(),
            incarnation: current.incarnation,
            view: "validation-second-view".into(),
        },
    )
    .unwrap()
    {
        WorkspaceReply::Native {
            control: Some(control),
            ..
        } => control,
        reply => panic!("{reply:?}"),
    };
    assert_eq!(
        inspect_native(&client, &id).task.validation.status,
        TaskValidationStatus::Running
    );
    let mut second = attach_owned_fixture(&h, &terminal);
    second.wait_for("validation-started");
    let state = inspect_native(&client, &id);
    assert!(request(
        &client,
        WorkspaceCommand::ControlNative {
            control: control.clone(),
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::MutateTask {
                expected_sequence: state.sequence,
                mutation: NativeTaskMutation::Complete
            })
        }
    )
    .is_err());
    // A repeated admission after reattachment uses the original operation ID;
    // it may return its current snapshot but must not start a second command.
    let replay = WorkspaceCommand::ControlNative {
        control: control.clone(),
        operation,
        action: Box::new(NativeAction::Validate {
            expected_sequence: before.sequence,
        }),
    };
    request(&client, replay).unwrap();
    assert_eq!(
        std::fs::read_to_string(h.root.join("validation-count"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    std::fs::write(h.root.join("validation-gate"), "").unwrap();
    second.wait_for("Validation exited:");
    second.input(b"\x04");
    second.wait_exit();
    let passed = wait_native(&client, &id, |s| {
        s.task.validation.status == TaskValidationStatus::Passed && !s.ownership_locked
    });
    assert!(passed
        .validation
        .as_ref()
        .unwrap()
        .output
        .contains("validation-final"));
    let complete = WorkspaceCommand::ControlNative {
        control: control.clone(),
        operation: uuid::Uuid::new_v4().to_string(),
        action: Box::new(NativeAction::MutateTask {
            expected_sequence: passed.sequence,
            mutation: NativeTaskMutation::Complete,
        }),
    };
    request(&client, complete.clone()).unwrap();
    request(&client, complete).unwrap();
    assert_eq!(
        inspect_native(&client, &id).task.status,
        TaskStatus::Completed
    );
    assert!(request(
        &client,
        WorkspaceCommand::ControlNative {
            control: control.clone(),
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::MutateTask {
                expected_sequence: passed.sequence,
                mutation: NativeTaskMutation::Archive
            })
        }
    )
    .is_err());
    let completed = inspect_native(&client, &id);
    request(
        &client,
        WorkspaceCommand::ControlNative {
            control: control.clone(),
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::MutateTask {
                expected_sequence: completed.sequence,
                mutation: NativeTaskMutation::Archive,
            }),
        },
    )
    .unwrap();
    let archived = inspect_native(&client, &id);
    assert_eq!(archived.task.status, TaskStatus::Archived);
    let rows = match request(&client, WorkspaceCommand::Refresh).unwrap() {
        WorkspaceReply::Catalog { rows } => rows,
        reply => panic!("{reply:?}"),
    };
    assert!(rows.iter().find(|r| r.id == id).unwrap().archived);
    request(
        &client,
        WorkspaceCommand::ControlNative {
            control,
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::MutateTask {
                expected_sequence: archived.sequence,
                mutation: NativeTaskMutation::Restore,
            }),
        },
    )
    .unwrap();
    assert_eq!(
        inspect_native(&client, &id).task.status,
        TaskStatus::Completed
    );
}
#[test]
fn owned_validation_descriptor_cannot_create_a_process_or_retarget_an_epoch() {
    let h = Harness::new();
    let descriptor = OwnedTerminalDescriptor {
        id: ConversationId("jterm:validation:missing".into()),
        epoch: uuid::Uuid::new_v4().to_string(),
        cwd: h.root.clone(),
        label: "not an executable".into(),
    };
    let mut view = attach_owned_fixture(&h, &descriptor);
    view.wait_for("historical");
    assert_ne!(view.wait_exit(), 0);
    assert!(!h.trace.exists());
}

#[test]
fn validation_failure_stays_unaccepted_and_rejects_unrestricted_task_overwrite() {
    use jterm_core::agent_task::{TaskStatus, TaskValidationStatus};
    let (_h, client, id, control, _) =
        stopped_native_validation_fixture("printf validation-failed; false".into());
    let before = inspect_native(&client, &id);
    request(
        &client,
        WorkspaceCommand::ControlNative {
            control: control.clone(),
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::Validate {
                expected_sequence: before.sequence,
            }),
        },
    )
    .unwrap();
    let failed = wait_native(&client, &id, |s| {
        s.task.validation.status == TaskValidationStatus::Failed && !s.ownership_locked
    });
    assert_eq!(failed.task.status, TaskStatus::ReadyForReview);
    assert_eq!(failed.task.validation.exit_code, Some(1));
    assert!(failed
        .validation
        .as_ref()
        .unwrap()
        .output
        .contains("validation-failed"));
    for mutation in [
        NativeTaskMutation::Complete,
        NativeTaskMutation::AcceptReviewed,
    ] {
        let current = inspect_native(&client, &id);
        assert!(
            request(
                &client,
                WorkspaceCommand::ControlNative {
                    control: control.clone(),
                    operation: uuid::Uuid::new_v4().to_string(),
                    action: Box::new(NativeAction::MutateTask {
                        expected_sequence: current.sequence,
                        mutation,
                    }),
                },
            )
            .is_err(),
            "validation failure was accepted via {mutation:?}"
        );
    }
    let mut forged = inspect_native(&client, &id).task;
    forged.status = TaskStatus::Completed;
    forged.source_context.as_mut().unwrap().command = Some("true".into());
    assert!(request(
        &client,
        WorkspaceCommand::ControlNative {
            control,
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::UpdateTask {
                task: Box::new(forged)
            }),
        },
    )
    .is_err());
    let retained = inspect_native(&client, &id);
    assert_eq!(retained.task.status, TaskStatus::ReadyForReview);
    assert_eq!(
        retained.task.validation.status,
        TaskValidationStatus::Failed
    );
}

#[test]
fn host_crash_marks_validation_unknown_and_never_repeats_the_surviving_command() {
    use jterm_core::agent_task::TaskValidationStatus;
    let (mut h, client, id, control, launch) = stopped_native_validation_fixture(
        "trap '' HUP TERM; printf 'boot %s %s\\n' \"$$\" \"$$\" >> FIXTURE_ROOT/trace; printf 'run\\n' >> FIXTURE_ROOT/validation-count; printf validation-survivor; while :; do sleep .05; done".into(),
    );
    let before = inspect_native(&client, &id);
    request(
        &client,
        WorkspaceCommand::ControlNative {
            control,
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::Validate {
                expected_sequence: before.sequence,
            }),
        },
    )
    .unwrap();
    let running = wait_native(&client, &id, |s| {
        s.validation
            .as_ref()
            .is_some_and(|v| v.terminal.is_some() && v.output.contains("validation-survivor"))
    });
    let terminal = running
        .validation
        .as_ref()
        .unwrap()
        .terminal
        .clone()
        .unwrap();
    let runtime = match request(
        &client,
        WorkspaceCommand::Runtime {
            id: terminal.id.clone(),
        },
    )
    .unwrap()
    {
        WorkspaceReply::Runtime { snapshot } => snapshot,
        reply => panic!("{reply:?}"),
    };
    h.host.kill().unwrap();
    h.host.wait().unwrap();
    assert!(
        process_live(runtime.pid),
        "synthetic validator did not retain its lease after host crash"
    );
    h.host = Command::new(host_executable())
        .arg("--jterm-session-host")
        .env("JTERM_WORKSPACE_STATE_DIR", &h.state)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            File::create(h.root.join("validation-restarted-host-errors")).unwrap(),
        ))
        .spawn()
        .unwrap();
    let recovered = wait_native(&h.client(), &id, |s| s.interrupted);
    assert!(!recovered.live);
    assert_eq!(
        recovered.task.validation.status,
        TaskValidationStatus::Inconclusive
    );
    assert_eq!(recovered.task.validation.exit_code, None);
    assert!(recovered.validation.as_ref().unwrap().terminal.is_none());
    assert!(!recovered.validation.as_ref().unwrap().preparing);
    assert!(
        request(
            &h.client(),
            WorkspaceCommand::AttachNative {
                id: id.clone(),
                incarnation: recovered.incarnation,
                view: "after-validation-crash".into()
            }
        )
        .is_err(),
        "surviving validation admitted a second task writer"
    );
    let mut missing_owner = attach_owned_fixture(&h, &terminal);
    missing_owner.wait_for("historical");
    assert_ne!(missing_owner.wait_exit(), 0);
    request(
        &h.client(),
        WorkspaceCommand::StartNative {
            launch: Box::new(launch),
        },
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(h.root.join("validation-count"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(
        process_live(runtime.pid),
        "restarted host signalled an unowned process"
    );
    // Cleanup only the still-identical local fixture group. Its inherited task
    // lease must never be mistaken for a stale timeout while it is alive.
    let args = std::fs::read(format!("/proc/{}/cmdline", runtime.pid)).unwrap();
    assert!(String::from_utf8_lossy(&args).contains(h.root.to_string_lossy().as_ref()));
    unsafe {
        libc::kill(-(runtime.pid as i32), libc::SIGKILL);
    }
}

#[test]
fn remote_validation_rerun_opens_only_the_newly_admitted_owned_terminal() {
    use jterm_core::agent_task::{AgentRuntimeManager, TaskManager, TaskValidationStatus};
    let (h, client, id, control, launch) = stopped_native_validation_fixture(
        "printf 'run\\n' >> FIXTURE_ROOT/validation-count; printf validation-completed".into(),
    );
    let current = inspect_native(&client, &id);
    request(
        &client,
        WorkspaceCommand::ControlNative {
            control: control.clone(),
            operation: uuid::Uuid::new_v4().to_string(),
            action: Box::new(NativeAction::Validate {
                expected_sequence: current.sequence,
            }),
        },
    )
    .unwrap();
    let previous = wait_native(&client, &id, |s| {
        s.task.validation.status == TaskValidationStatus::Passed && !s.ownership_locked
    });
    let previous_terminal = previous.validation.unwrap().terminal.unwrap();
    request(&client, WorkspaceCommand::DetachNative { control }).unwrap();
    // Start from a fresh frontend cache, with no control lease. The first
    // AttachNative reply still includes the previous validation descriptor.
    let mut view = AgentRuntimeManager::hosted_with_client("validation-observer", h.client());
    let mut tasks = TaskManager::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    while view.hosted_validation_terminal(launch.task.id).is_none() {
        view.poll(&mut tasks, launch.policy);
        assert!(
            Instant::now() < deadline,
            "view never received prior validation history"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    view.start_hosted_validation(launch.task.id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let newly_admitted = loop {
        let report = view.poll(&mut tasks, launch.policy);
        assert!(
            report.issues.is_empty(),
            "remote validation was rejected: {report:?}"
        );
        let launches = view.take_owned_terminal_launches();
        assert!(
            launches
                .iter()
                .all(|(_, terminal)| terminal.id != previous_terminal.id),
            "an AttachNative reply opened the previous validation terminal"
        );
        if let Some((task, terminal)) = launches.into_iter().next() {
            assert_eq!(task, launch.task.id);
            break terminal;
        }
        assert!(
            Instant::now() < deadline,
            "new validation terminal was never emitted"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_ne!(newly_admitted.epoch, previous_terminal.epoch);
    wait_native(&client, &id, |s| {
        s.task.validation.status == TaskValidationStatus::Passed && !s.ownership_locked
    });
    assert_eq!(
        std::fs::read_to_string(h.root.join("validation-count"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn remote_validation_reports_missing_directory_without_running_in_a_fallback_cwd() {
    use jterm_core::agent_task::{AgentRuntimeManager, TaskManager, TaskValidationStatus};
    let (h, client, id, control, launch) = stopped_native_validation_fixture(
        "printf forbidden-fallback >> FIXTURE_ROOT/validation-count".into(),
    );
    std::fs::rename(&launch.task.worktree_path, h.root.join("moved-worktree")).unwrap();
    request(&client, WorkspaceCommand::DetachNative { control }).unwrap();
    let mut view =
        AgentRuntimeManager::hosted_with_client("missing-directory-observer", h.client());
    let mut tasks = TaskManager::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    while view.conversation_id(launch.task.id).is_none() {
        view.poll(&mut tasks, launch.policy);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    view.start_hosted_validation(launch.task.id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let issue = loop {
        let report = view.poll(&mut tasks, launch.policy);
        assert!(
            view.take_owned_terminal_launches().is_empty(),
            "missing cwd created a terminal"
        );
        if let Some(issue) = report
            .issues
            .into_iter()
            .find(|issue| issue.task_id == launch.task.id)
        {
            break issue;
        }
        assert!(
            Instant::now() < deadline,
            "validation preflight failure was never reported to the view"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        issue.detail.contains("worktree"),
        "missing directory repair detail lost: {issue:?}"
    );
    assert!(
        !h.root.join("validation-count").exists(),
        "validation ran in a fallback directory"
    );
    let historical = inspect_native(&client, &id);
    assert!(!historical.ownership_locked);
    assert_eq!(
        historical.task.validation.status,
        TaskValidationStatus::Inconclusive
    );
    assert!(historical.validation.as_ref().unwrap().terminal.is_none());
    assert!(view.hosted_validation_snapshot(launch.task.id).is_some());
}

#[test]
fn remote_validation_acknowledges_stricter_privacy_before_starting_the_command() {
    use jterm_core::agent_task::{
        AgentRuntimeManager, NativePromptPolicy, TaskManager, TaskValidationStatus,
    };
    let secret = "synthetic-validation-policy-secret-123456";
    let (h, client, id, control, launch) = stopped_native_validation_fixture_with_redaction(
        format!("printf 'API_TOKEN={secret}'; printf 'run\\n' >> FIXTURE_ROOT/validation-count"),
        false,
    );
    let old = inspect_native(&client, &id);
    assert!(!old.redact_secrets);
    assert!(old.task.source_context.as_ref().unwrap().command_exact);
    request(&client, WorkspaceCommand::DetachNative { control }).unwrap();
    let policy = NativePromptPolicy {
        redact_secrets: true,
        ..launch.policy
    };
    let mut view =
        AgentRuntimeManager::hosted_with_client("privacy-validation-observer", h.client());
    let mut tasks = TaskManager::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    while view.conversation_id(launch.task.id).is_none() {
        view.poll(&mut tasks, policy);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    view.start_hosted_validation(launch.task.id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let report = view.poll(&mut tasks, policy);
        assert!(
            report.issues.is_empty(),
            "privacy/CAS handshake failed: {report:?}"
        );
        if !view.take_owned_terminal_launches().is_empty() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let completed = wait_native(&client, &id, |s| {
        s.task.validation.status == TaskValidationStatus::Passed && !s.ownership_locked
    });
    assert!(completed.redact_secrets);
    assert!(completed.validation_evidence_redacted);
    assert!(
        !completed
            .task
            .source_context
            .as_ref()
            .unwrap()
            .command_exact
    );
    assert!(!completed
        .validation
        .as_ref()
        .unwrap()
        .output
        .contains(secret));
    assert_eq!(
        std::fs::read_to_string(h.root.join("validation-count"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}
