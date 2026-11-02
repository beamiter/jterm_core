#![cfg(target_os = "linux")]
//! A stub SSH executable exercises the real JSON bridge and an isolated host.
//! These tests do not connect to a network or run a real model/provider.
use jterm_core::agent_workspace::*;
use jterm_core::supervised::SupervisedChild;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const HOST: &str = env!("CARGO_BIN_EXE_jterm-sessiond");
fn host_executable() -> PathBuf {
    std::env::var_os("JTERM_TEST_HOST_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|| HOST.into())
}

struct Fixture {
    root: PathBuf,
    state: PathBuf,
    host: Option<SupervisedChild>,
}
impl Fixture {
    fn new(start_host: bool) -> Self {
        let root = std::env::temp_dir().join(format!("jw-ssh-{}", uuid::Uuid::new_v4().simple()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let state = root.join("state");
        let host = start_host.then(|| {
            let mut command = Command::new(host_executable());
            command
                .arg("--jterm-session-host")
                .env("JTERM_WORKSPACE_STATE_DIR", &state)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            SupervisedChild::spawn(&mut command).unwrap()
        });
        let fixture = Self { root, state, host };
        if start_host {
            let client = fixture.local();
            let until = Instant::now() + Duration::from_secs(5);
            while request(&client, WorkspaceCommand::Refresh).is_err() {
                assert!(Instant::now() < until, "fixture host did not become ready");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        fixture
    }
    fn local(&self) -> WorkspaceClient {
        WorkspaceClient::for_state_dir(&self.state)
    }
    fn script(&self, name: &str, body: &str) -> PathBuf {
        let script = self.root.join(name);
        std::fs::write(&script, format!("#!/usr/bin/python3\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        script
    }
    fn remote(&self, executable: &Path, expected_host: &str) -> WorkspaceClient {
        let config = WorkspaceSshHost::new("fixture-host", host_executable(), expected_host)
            .unwrap()
            .with_ssh_executable(executable)
            .unwrap();
        WorkspaceClient::with_ssh_host(config).unwrap()
    }
    fn bridge(&self) -> PathBuf {
        let expected = json!([
            "-T",
            "-oBatchMode=yes",
            "-oStrictHostKeyChecking=yes",
            "-oConnectTimeout=10",
            "-oServerAliveInterval=5",
            "-oServerAliveCountMax=2",
            "-oClearAllForwardings=yes",
            "-oForwardAgent=no",
            "-oForwardX11=no",
            "-oPermitLocalCommand=no",
            "--",
            "fixture-host",
            host_executable(),
            "--jterm-command-stdio"
        ]);
        self.script("ssh-bridge", &format!(
            "import os, sys\nassert sys.argv[1:] == {expected}\nwith open({}, 'a') as f: f.write('x')\nos.environ['JTERM_WORKSPACE_STATE_DIR'] = {}\nos.execv({}, [{}, '--jterm-command-stdio'])",
            py(self.root.join("attempts")), py(&self.state), py(host_executable()), py(host_executable())
        ))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        drop(self.host.take());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn py(value: impl AsRef<Path>) -> String {
    serde_json::to_string(&value.as_ref().to_string_lossy()).unwrap()
}
fn request(client: &WorkspaceClient, command: WorkspaceCommand) -> Result<WorkspaceReply, String> {
    let id = client.send(command)?;
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        for WorkspaceEvent::Reply { request_id, result } in client.poll() {
            if request_id == id {
                return result;
            }
        }
        if Instant::now() >= until {
            return Err("fixture request deadline exceeded".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn ssh_bridge_reads_exact_remote_catalog_and_search_without_local_activation() {
    let fixture = Fixture::new(true);
    let local = fixture.local();
    let id = source_id(
        "Codex",
        &local_host_id(),
        Path::new("/fixture/remote-profile"),
        "remote-thread",
    )
    .unwrap();
    request(
        &local,
        WorkspaceCommand::IndexSessions {
            sessions: vec![WorkspaceRow {
                id: id.clone(),
                kind: ConversationKind::Cli,
                title: "Remote fixture".into(),
                provider: "Codex".into(),
                cwd: "/remote/project".into(),
                preview: String::new(),
                updated_at_ms: 1,
                archived: false,
                pinned: false,
                tags: Vec::new(),
                runtime: RuntimeState::Idle,
                facts: WorkspaceFacts::default(),
            }],
        },
    )
    .unwrap();
    request(
        &local,
        WorkspaceCommand::IndexText {
            documents: vec![WorkspaceTextDocument {
                conversation: id.clone(),
                event_id: "external:fixture".into(),
                revision: "r1".into(),
                source: SearchSource::ProviderHistory,
                message_sequence: None,
                text: "Remote read-only evidence 中文".into(),
                truncated: false,
            }],
        },
    )
    .unwrap();
    let remote = fixture.remote(&fixture.bridge(), &local_host_id());
    assert!(remote.is_read_only());
    let WorkspaceReply::CatalogPage { rows, .. } = request(
        &remote,
        WorkspaceCommand::CatalogPage {
            before: None,
            limit: 10,
        },
    )
    .unwrap() else {
        panic!("catalog")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].id, id,
        "remote identity is retained, never rewritten as a local source"
    );
    let WorkspaceReply::Search { page } = request(
        &remote,
        WorkspaceCommand::Search {
            request: WorkspaceSearchRequest {
                query: "中文".into(),
                limit: 10,
                ..Default::default()
            },
        },
    )
    .unwrap() else {
        panic!("search")
    };
    assert_eq!(page.hits.len(), 1);
    let anchor = &page.hits[0].anchor;
    let WorkspaceReply::SearchDocument { document } = request(
        &remote,
        WorkspaceCommand::ReadSearchDocument {
            conversation: id,
            event_id: anchor.event_id.clone(),
            revision: anchor.revision.clone(),
        },
    )
    .unwrap() else {
        panic!("document")
    };
    assert!(document.text.contains("中文"));
    // Shell-looking query text is a JSON field, never part of the SSH argv.
    let sentinel = fixture.root.join("should-not-exist");
    request(
        &remote,
        WorkspaceCommand::Search {
            request: WorkspaceSearchRequest {
                query: format!("\"$(touch {})\"", sentinel.display()),
                limit: 10,
                ..Default::default()
            },
        },
    )
    .unwrap();
    assert!(!sentinel.exists());
    let attempts = std::fs::read(fixture.root.join("attempts")).unwrap();
    assert!(remote
        .send(WorkspaceCommand::CreateChat {
            title: "Forbidden".into()
        })
        .is_err());
    assert_eq!(
        std::fs::read(fixture.root.join("attempts")).unwrap(),
        attempts,
        "mutations are rejected before SSH starts"
    );
}

#[test]
fn ssh_host_mismatch_prevents_query_transmission_and_connection_loss_is_not_retried() {
    let fixture = Fixture::new(false);
    let received = fixture.root.join("received");
    let counter = fixture.root.join("attempts");
    let mismatch = fixture.script("ssh-mismatch", &format!(
        "import json, sys\nwith open({}, 'a') as f: f.write('x')\nprint(json.dumps({{'version': {}, 'host_id': 'wrong-host', 'mode': 'read-only'}}), flush=True)\nrequest = sys.stdin.buffer.read()\nif request:\n    with open({}, 'wb') as f: f.write(request)",
        py(&counter), PROTOCOL_VERSION, py(&received)
    ));
    let remote = fixture.remote(&mismatch, "expected-host");
    assert!(request(&remote, WorkspaceCommand::Refresh)
        .unwrap_err()
        .contains("identity mismatch"));
    assert!(
        !received.exists(),
        "query text crossed a mismatched host handshake"
    );
    assert_eq!(std::fs::read(&counter).unwrap(), b"x");

    let disconnected = fixture.script("ssh-disconnected", &format!(
        "import json, sys\nwith open({}, 'a') as f: f.write('x')\nprint(json.dumps({{'version': {}, 'host_id': 'expected-host', 'mode': 'read-only'}}), flush=True)\nrequest = sys.stdin.buffer.readline()\nwith open({}, 'wb') as f: f.write(request)\nsys.exit(255)",
        py(&counter), PROTOCOL_VERSION, py(&received)
    ));
    let remote = fixture.remote(&disconnected, "expected-host");
    assert!(request(&remote, WorkspaceCommand::Refresh)
        .unwrap_err()
        .contains("not retried"));
    let request: Value = serde_json::from_slice(&std::fs::read(&received).unwrap()).unwrap();
    assert_eq!(request["command"], "Refresh");
    assert_eq!(
        std::fs::read(&counter).unwrap(),
        b"xx",
        "a lost reply must not spawn a retry"
    );
}

fn stdio(fixture: &Fixture, command: WorkspaceCommand) -> Value {
    let mut child = Command::new(host_executable())
        .arg("--jterm-command-stdio")
        .env("JTERM_WORKSPACE_STATE_DIR", &fixture.state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    let hello: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(hello["mode"], "read-only");
    let frame = json!({"version":PROTOCOL_VERSION,"expected_host_id":hello["host_id"],
        "request_id":uuid::Uuid::new_v4().to_string(),"command":command});
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{frame}").unwrap();
    drop(input);
    line.clear();
    output.read_line(&mut line).unwrap();
    assert!(child.wait().unwrap().success());
    serde_json::from_str(&line).unwrap()
}

#[test]
fn stdio_bridge_rejects_mutation_and_never_autostarts_an_absent_host() {
    let fixture = Fixture::new(false);
    let response = stdio(
        &fixture,
        WorkspaceCommand::CreateChat {
            title: "Must not be created".into(),
        },
    );
    assert!(response["result"]["Err"]
        .as_str()
        .unwrap()
        .contains("read-only"));
    assert!(!fixture.state.exists());
    let response = stdio(&fixture, WorkspaceCommand::Refresh);
    assert!(response["result"]["Err"]
        .as_str()
        .unwrap()
        .contains("not running"));
    assert!(
        !fixture.state.exists(),
        "read-only observation created a host or state database"
    );
    let identity = Command::new(host_executable())
        .arg("--jterm-workspace-host-id")
        .output()
        .unwrap();
    assert!(identity.status.success());
    assert_eq!(
        String::from_utf8(identity.stdout).unwrap().trim(),
        local_host_id()
    );
}
