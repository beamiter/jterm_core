use super::model::*;
use super::ownership::*;
use super::protocol::*;
use super::store::Store;
use super::PROTOCOL_VERSION;
use crate::agent_sessions::{AgentSession, SessionProvider, SessionStatus};
use crate::screen_replay::ScreenReplay;
use crate::supervised::SupervisedChild;
use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_RUNTIMES: usize = 16;
const MAX_CLIENTS: usize = 64;
const MAX_INPUT: usize = 64 * 1024;

enum AttachTarget {
    Provider(Box<ManagedLaunch>),
    Owned(super::native::OwnedTerminalDescriptor, u16, u16),
}
enum HostRequest {
    MissionInspected(
        super::WorkspaceMission,
        super::WorkspaceMissionReport,
        Sender<Result<WorkspaceReply, String>>,
    ),
    Command(
        WorkspaceCommand,
        (u32, String),
        Sender<Result<WorkspaceReply, String>>,
    ),
    Attach(
        AttachTarget,
        String,
        Sender<Response>,
        Arc<AtomicBool>,
        Sender<Result<RuntimeHandle, String>>,
    ),
}

#[derive(Clone)]
pub(super) struct RuntimeHandle {
    pub(super) control: Sender<Control>,
    pub(super) snapshot: Arc<Mutex<RuntimeSnapshot>>,
    pub(super) output: Arc<Mutex<RuntimeOutput>>,
}
#[derive(Default)]
pub(super) struct RuntimeOutput {
    pub raw: VecDeque<u8>,
    pub screen: Vec<u8>,
    pub truncated: bool,
}
impl RuntimeHandle {
    pub(super) fn stop(&self) -> Result<(), String> {
        self.control
            .try_send(Control::Stop)
            .map_err(|_| "Runtime is busy or already stopped".into())
    }
}

pub(super) enum Control {
    Attach {
        client: String,
        output: Sender<Response>,
        closed: Arc<AtomicBool>,
        cols: u16,
        rows: u16,
        ack: Sender<Result<(), String>>,
    },
    Input {
        client: String,
        epoch: String,
        bytes: Vec<u8>,
    },
    Resize {
        client: String,
        epoch: String,
        cols: u16,
        rows: u16,
    },
    Detach {
        client: String,
        epoch: String,
    },
    Stop,
}

struct CloseAttachment(Arc<AtomicBool>);
impl Drop for CloseAttachment {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Run the foreground host. The CLI dispatcher daemonizes before invoking
/// this function. Only this thread touches SQLite; PTY workers never do.
pub(super) fn run(paths: Paths) -> Result<(), String> {
    let mut singleton = Lease::acquire(&paths.runtime.join("host.lock"))?;
    singleton.write(&(std::process::id(), process_start(std::process::id())))?;
    if let Ok(metadata) = std::fs::symlink_metadata(&paths.socket) {
        use std::os::unix::fs::FileTypeExt;
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err("Unsafe session host socket path".into());
        }
        // Only the singleton lease owner may unlink a stale socket.
        std::fs::remove_file(&paths.socket).map_err(|e| e.to_string())?;
    }
    let listener = UnixListener::bind(&paths.socket).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let mut store = Store::open(&paths)?;
    let (tx, rx) = bounded::<HostRequest>(64);
    let clients = Arc::new(AtomicUsize::new(0));
    let mut runtimes: HashMap<ConversationId, RuntimeHandle> = HashMap::new();
    let mut api = super::api_host::ApiManager::new(paths.clone());
    let mut native = super::native_host::NativeManager::new(paths.clone());
    let result_workers = Arc::new(AtomicUsize::new(0));
    let mut last_index_batch = Instant::now();
    loop {
        api.poll(&mut store);
        native.poll(&mut store, &mut runtimes);
        if last_index_batch.elapsed() >= Duration::from_millis(500) {
            let _ = store.advance_search_index();
            last_index_batch = Instant::now();
        }
        for _ in 0..8 {
            match listener.accept() {
                Ok((stream, _)) => {
                    if clients.load(Ordering::Relaxed) >= MAX_CLIENTS || !peer_is_same_user(&stream)
                    {
                        drop(stream);
                        continue;
                    }
                    clients.fetch_add(1, Ordering::Relaxed);
                    let count = clients.clone();
                    let tx = tx.clone();
                    std::thread::spawn(move || {
                        let _ = connection(stream, tx);
                        count.fetch_sub(1, Ordering::Relaxed);
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.to_string()),
            }
        }
        match rx.recv_timeout(Duration::from_millis(15)) {
            Ok(HostRequest::MissionInspected(mission, report, reply)) => {
                let result = store.record_mission_report(mission, report);
                let _ = reply.try_send(result);
            }
            Ok(HostRequest::Command(command, owner, reply)) => {
                if let WorkspaceCommand::InspectMission { id } = &command {
                    let inputs = (|| {
                        let mission = store.read_mission(id)?;
                        let mut snapshots = Vec::new();
                        for repository in &mission.repositories {
                            for task in &repository.tasks {
                                if let Some(snapshot) = store.native_snapshot(&task.conversation)? {
                                    snapshots.push(snapshot);
                                }
                            }
                        }
                        Ok::<_, String>((mission, snapshots))
                    })();
                    match inputs {
                        Err(error) => {
                            let _ = reply.try_send(Err(error));
                        }
                        Ok((mission, snapshots)) => {
                            if result_workers.load(Ordering::Relaxed) >= 4 {
                                let _ = reply.try_send(Err("Evidence inspection is busy; retry when another review finishes".into()));
                                continue;
                            }
                            result_workers.fetch_add(1, Ordering::Relaxed);
                            let count = result_workers.clone();
                            let completion = tx.clone();
                            let worker_reply = reply.clone();
                            if let Err(error) = std::thread::Builder::new()
                                .name("jterm-mission-review".into())
                                .spawn(move || {
                                    let report = super::mission_host::inspect(&mission, &snapshots);
                                    count.fetch_sub(1, Ordering::Relaxed);
                                    if let Err(error) =
                                        completion.try_send(HostRequest::MissionInspected(
                                            mission,
                                            report,
                                            worker_reply.clone(),
                                        ))
                                    {
                                        let _ = worker_reply.try_send(Err(format!(
                                            "Mission inspection could not be persisted: {error}"
                                        )));
                                    }
                                })
                            {
                                result_workers.fetch_sub(1, Ordering::Relaxed);
                                let _ = reply.try_send(Err(error.to_string()));
                            }
                        }
                    }
                    continue;
                }
                if let WorkspaceCommand::InspectResult { id } = &command {
                    let snapshot = store.native_snapshot(id).and_then(|value| {
                        value.ok_or_else(|| {
                            "No hosted task result exists for this conversation".to_string()
                        })
                    });
                    match snapshot {
                        Err(error) => {
                            let _ = reply.try_send(Err(error));
                        }
                        Ok(snapshot) => {
                            if result_workers.load(Ordering::Relaxed) >= 4 {
                                let _ = reply.try_send(Err("Result inspection is busy; retry after another review finishes".into()));
                                continue;
                            }
                            result_workers.fetch_add(1, Ordering::Relaxed);
                            let count = result_workers.clone();
                            let worker_reply = reply.clone();
                            if let Err(error) = std::thread::Builder::new()
                                .name("jterm-result-review".into())
                                .spawn(move || {
                                    let report = super::WorkspaceResultReport::inspect(&snapshot);
                                    count.fetch_sub(1, Ordering::Relaxed);
                                    let _ = worker_reply.try_send(Ok(WorkspaceReply::Result {
                                        report: Box::new(report),
                                    }));
                                })
                            {
                                result_workers.fetch_sub(1, Ordering::Relaxed);
                                let _ = reply.try_send(Err(error.to_string()));
                            }
                        }
                    }
                    continue;
                }
                let response = (|| {
                    store.reap_abandoned_requests()?;
                    let command = match api.handle(command, owner.clone(), &mut store) {
                        super::api_host::Disposition::Handled(reply) => return *reply,
                        super::api_host::Disposition::Other(command) => command,
                    };
                    let command = match native.handle(command, owner.clone(), &mut store) {
                        super::api_host::Disposition::Handled(reply) => return *reply,
                        super::api_host::Disposition::Other(command) => command,
                    };
                    match command {
                        WorkspaceCommand::Refresh => store.rows().map(|mut rows| {
                            for row in &mut rows {
                                project_runtime_row(row, &runtimes, &paths);
                            }
                            WorkspaceReply::Catalog { rows }
                        }),
                        WorkspaceCommand::CatalogPage { before, limit } => {
                            let mut reply = store.catalog_page(before, limit)?;
                            if let WorkspaceReply::CatalogPage { rows, .. } = &mut reply {
                                for row in rows.iter_mut() {
                                    project_runtime_row(row, &runtimes, &paths);
                                }
                            }
                            Ok(reply)
                        }
                        WorkspaceCommand::Search { request } => {
                            let page = store.search_with(&request, |row| {
                                project_runtime_row(row, &runtimes, &paths)
                            })?;
                            Ok(WorkspaceReply::Search { page })
                        }
                        WorkspaceCommand::Runtime { id } => runtimes
                            .get(&id)
                            .map(|r| WorkspaceReply::Runtime {
                                snapshot: r.snapshot.lock().clone(),
                            })
                            .ok_or_else(|| "No managed runtime for this conversation".into()),
                        WorkspaceCommand::Stop { id } => match runtimes.get(&id) {
                            Some(r) if r.snapshot.lock().state.is_live() => r
                                .control
                                .try_send(Control::Stop)
                                .map(|_| WorkspaceReply::Ack)
                                .map_err(|_| "Runtime control queue is busy; retry Stop".into()),
                            Some(_) => Ok(WorkspaceReply::Ack),
                            None => Err("This process is not owned by the session host".into()),
                        },
                        WorkspaceCommand::Delete { ref id }
                        | WorkspaceCommand::Archive { ref id, .. }
                            if runtimes
                                .get(id)
                                .is_some_and(|r| r.snapshot.lock().state.is_live()) =>
                        {
                            Err(
                                "Stop the managed runtime before archiving or deleting its row"
                                    .into(),
                            )
                        }
                        WorkspaceCommand::Organize { ref changes }
                            if changes.iter().any(|change| {
                                change.archived.is_some()
                                    && runtimes
                                        .get(&change.id)
                                        .is_some_and(|r| r.snapshot.lock().state.is_live())
                            }) =>
                        {
                            Err("Stop running sessions before applying an archive batch".into())
                        }
                        other => store.command_as(other, owner),
                    }
                })();
                let _ = reply.try_send(response);
            }
            Ok(HostRequest::Attach(target, client, output, closed, reply)) => {
                let result = (|| {
                    let launch = match target {
                        AttachTarget::Provider(launch) => launch,
                        AttachTarget::Owned(terminal, cols, rows) => {
                            if !terminal.id.0.starts_with("jterm:validation:")
                                || uuid::Uuid::parse_str(&terminal.epoch).is_err()
                            {
                                return Err("Invalid owned runtime identity".into());
                            }
                            let runtime=runtimes.get(&terminal.id).ok_or("This validation runtime is historical; its outcome is available in the task view and no process was restarted")?.clone();
                            if runtime.snapshot.lock().epoch != terminal.epoch {
                                return Err("Stale owned runtime epoch".into());
                            }
                            return attach(runtime, cols, rows, client, output, closed);
                        }
                    };
                    validate_launch(&launch)?;
                    let id = launch.row.id.clone();
                    if let Some(runtime) = runtimes.get(&id) {
                        if runtime.snapshot.lock().state.is_live() {
                            return attach(
                                runtime.clone(),
                                launch.cols,
                                launch.rows,
                                client,
                                output,
                                closed,
                            );
                        }
                    }
                    if runtimes
                        .values()
                        .filter(|r| r.snapshot.lock().state.is_live())
                        .count()
                        >= MAX_RUNTIMES
                    {
                        return Err("Session host has reached its 16 running-session limit".into());
                    }
                    // Keep completed status entries bounded, independently of
                    // durable rows stored in SQLite.
                    if runtimes.len() >= MAX_RUNTIMES * 2 {
                        runtimes.retain(|_, r| r.snapshot.lock().state.is_live());
                    }
                    if let Some(pid) = external_owner(&launch) {
                        return Err(format!("External process {pid} owns this exact provider session. Close or stop it before resuming here."));
                    }
                    store.upsert_runtime_row(&launch.row)?;
                    let runtime = spawn_runtime(&paths, &launch)?;
                    runtimes.insert(id, runtime.clone());
                    attach(runtime, launch.cols, launch.rows, client, output, closed)
                })();
                let _ = reply.try_send(result);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn attach(
    runtime: RuntimeHandle,
    cols: u16,
    rows: u16,
    client: String,
    output: Sender<Response>,
    closed: Arc<AtomicBool>,
) -> Result<RuntimeHandle, String> {
    let snapshot = runtime.snapshot.lock().clone();
    if !snapshot.state.is_live() {
        output
            .try_send(Response::Attached {
                snapshot: snapshot.clone(),
                screen: runtime.output.lock().screen.clone(),
                truncated: true,
            })
            .map_err(|_| "Attachment closed")?;
        output
            .try_send(Response::Exited {
                sequence: snapshot.event_sequence,
                code: snapshot.exit_code,
            })
            .map_err(|_| "Attachment closed")?;
        return Ok(runtime);
    }
    let (ack, rx) = bounded(1);
    runtime
        .control
        .try_send(Control::Attach {
            client,
            output,
            closed,
            cols,
            rows,
            ack,
        })
        .map_err(|_| "Runtime is busy or has exited")?;
    rx.recv_timeout(Duration::from_secs(3))
        .map_err(|_| "Runtime attach timed out")??;
    Ok(runtime)
}

fn project_runtime_row(
    row: &mut WorkspaceRow,
    runtimes: &HashMap<ConversationId, RuntimeHandle>,
    paths: &Paths,
) {
    if let Some(runtime) = runtimes.get(&row.id) {
        row.runtime = runtime.snapshot.lock().state;
        if row.kind == ConversationKind::Cli {
            row.facts.capabilities = super::WorkspaceCapabilities::cli();
        }
    } else if row.kind == ConversationKind::Cli {
        row.runtime = if lease_is_held(&paths.lease(&row.id))
            && read_lease(&paths.lease(&row.id)).is_some_and(|record| {
                record.id == row.id
                    && process_matches(record.runtime.pid, &record.runtime.process_start)
            }) {
            RuntimeState::Conflict
        } else {
            RuntimeState::Idle
        };
        // Capability comes from a host-supported adapter, not scanned history.
        if matches!(row.provider.as_str(), "Codex" | "Claude") {
            row.facts.capabilities = super::WorkspaceCapabilities::cli();
        }
    }
}

fn connection(mut stream: UnixStream, host: Sender<HostRequest>) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    match read_frame::<Request>(&mut reader)?.ok_or("Empty request")? {
        Request::Command { version, command } => {
            if version != PROTOCOL_VERSION {
                return write_frame(&mut stream,&Response::Error("Session host version mismatch; restart the host after updating all frontends".into()));
            }
            let owner = peer_identity(&stream).ok_or("Cannot verify workspace client process")?;
            let (tx, rx) = bounded(1);
            let timeout = if matches!(&command, WorkspaceCommand::InspectMission { .. }) {
                115
            } else if matches!(&command, WorkspaceCommand::InspectResult { .. }) {
                55
            } else {
                8
            };
            host.try_send(HostRequest::Command(command, owner, tx))
                .map_err(|_| "Session host request queue is busy")?;
            let result = rx
                .recv_timeout(Duration::from_secs(timeout))
                .map_err(|_| "Workspace request timed out")?;
            write_frame(&mut stream, &Response::Reply(Box::new(result)))
        }
        request @ (Request::Attach { .. } | Request::AttachOwned { .. }) => {
            let (version, target, client) = match request {
                Request::Attach {
                    version,
                    launch,
                    client,
                } => (version, AttachTarget::Provider(launch), client),
                Request::AttachOwned {
                    version,
                    terminal,
                    client,
                    cols,
                    rows,
                } => (version, AttachTarget::Owned(terminal, cols, rows), client),
                _ => unreachable!(),
            };
            if version != PROTOCOL_VERSION || client.len() > 128 {
                return write_frame(
                    &mut stream,
                    &Response::Error("Incompatible or invalid attachment".into()),
                );
            }
            let (output, events) = bounded(64);
            let (reply, rx) = bounded(1);
            let writer_closed = Arc::new(AtomicBool::new(false));
            let _close_attachment = CloseAttachment(writer_closed.clone());
            host.try_send(HostRequest::Attach(
                target,
                client.clone(),
                output,
                writer_closed.clone(),
                reply,
            ))
            .map_err(|_| "Host attach queue is busy")?;
            let runtime = match rx
                .recv_timeout(Duration::from_secs(8))
                .map_err(|_| "Host attach timed out")?
            {
                Ok(runtime) => runtime,
                Err(error) => return write_frame(&mut stream, &Response::Error(error)),
            };
            let epoch = runtime.snapshot.lock().epoch.clone();
            let output_stream = stream.try_clone().map_err(|e| e.to_string())?;
            let closed = writer_closed.clone();
            let writer = std::thread::spawn(move || {
                let mut output_stream = output_stream;
                while !closed.load(Ordering::Relaxed) {
                    let event = match events.recv_timeout(Duration::from_millis(50)) {
                        Ok(event) => event,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                        Err(_) => break,
                    };
                    let done = matches!(event, Response::Exited { .. } | Response::Error(_));
                    if write_frame(&mut output_stream, &event).is_err() || done {
                        break;
                    }
                }
                closed.store(true, Ordering::Relaxed);
                let _ = output_stream.shutdown(std::net::Shutdown::Both);
            });
            stream.set_read_timeout(None).map_err(|e| e.to_string())?;
            while let Ok(Some(request)) = read_frame::<Request>(&mut reader) {
                let control = match request {
                    Request::Input {
                        client: c,
                        epoch: e,
                        bytes,
                    } if c == client && e == epoch && bytes.len() <= OUTPUT_CHUNK_BYTES => {
                        Control::Input {
                            client: c,
                            epoch: e,
                            bytes,
                        }
                    }
                    Request::Resize {
                        client: c,
                        epoch: e,
                        cols,
                        rows,
                    } if c == client && e == epoch => Control::Resize {
                        client: c,
                        epoch: e,
                        cols,
                        rows,
                    },
                    Request::Detach {
                        client: c,
                        epoch: e,
                    } if c == client && e == epoch => break,
                    _ => break,
                };
                if runtime.control.try_send(control).is_err() {
                    break;
                }
            }
            let _ = runtime.control.send_timeout(
                Control::Detach { client, epoch },
                Duration::from_millis(100),
            );
            writer_closed.store(true, Ordering::Relaxed);
            let _ = stream.shutdown(std::net::Shutdown::Both);
            let _ = writer.join();
            Ok(())
        }
        _ => Err("Attach before sending terminal input".into()),
    }
}

pub(super) fn peer_is_same_user(stream: &UnixStream) -> bool {
    peer_identity(stream).is_some()
}

fn peer_identity(stream: &UnixStream) -> Option<(u32, String)> {
    #[cfg(target_os = "linux")]
    unsafe {
        let mut credentials: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut credentials as *mut _ as *mut libc::c_void,
            &mut len,
        ) != 0
            || credentials.uid != libc::geteuid()
        {
            return None;
        }
        let pid = u32::try_from(credentials.pid).ok()?;
        Some((pid, process_start(pid)?))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = stream;
        None
    }
}

fn validate_launch(launch: &ManagedLaunch) -> Result<(), String> {
    let provider = match launch.provider.as_str() {
        "codex" => SessionProvider::Codex,
        "claude" => SessionProvider::Claude,
        _ => return Err("Unsupported managed provider".into()),
    };
    let session = AgentSession {
        provider,
        id: launch.native_id.clone(),
        title: launch.row.title.clone(),
        cwd: launch.cwd.clone(),
        updated_at_ms: launch.row.updated_at_ms,
        status: SessionStatus::Unknown,
        preview: launch.row.preview.clone(),
        branch: None,
        source: launch.source.clone(),
        archived: launch.row.archived,
    };
    session.resume_launch()?;
    if session_id(&session)? != launch.row.id
        || session.provider_home().as_ref() != Some(&launch.provider_home)
        || launch.row.kind != ConversationKind::Cli
    {
        return Err("Managed launch source identity does not match its provider history".into());
    }
    Ok(())
}

fn spawn_runtime(paths: &Paths, launch: &ManagedLaunch) -> Result<RuntimeHandle, String> {
    spawn_runtime_with_native(paths, launch, None)
}
pub(super) fn spawn_runtime_with_native(
    paths: &Paths,
    launch: &ManagedLaunch,
    mut native: Option<super::native_handoff::NativeCliOptions>,
) -> Result<RuntimeHandle, String> {
    // Native app-server profiles are isolated and may have an owner before
    // their first thread ID is known. The profile lease bridges that phase
    // and is released only by an acknowledged native-to-CLI handoff.
    let profile_lease = if native.is_none()
        && launch.provider == "codex"
        && launch
            .provider_home
            .join("jterm-native-profile.json")
            .is_file()
    {
        Some(NativeSessionLease::acquire_at(
            &paths.state,
            SessionProvider::Codex,
            &launch.provider_home,
            "native-profile",
        )?)
    } else {
        None
    };
    if profile_lease.is_some() {
        return Err("Open the native task and choose Continue in Terminal to acquire a fresh access grant for this exact thread".into());
    }
    let mut lease = match native
        .as_mut()
        .and_then(|options| options.session_lease.take())
    {
        Some(lease) => lease.into_lease(),
        None => Lease::acquire(&paths.lease(&launch.row.id))?,
    };
    let (cols, rows) = dimensions(launch.cols, launch.rows);
    let (master, slave) = open_pty(cols, rows)?;
    let mut command = if let Some(options) = &native {
        let first = options
            .argv
            .first()
            .ok_or("Empty trusted native CLI launcher")?;
        let mut command = Command::new(first);
        command.args(&options.argv[1..]);
        command
    } else {
        Command::new(&launch.provider)
    };
    command
        .current_dir(&launch.cwd)
        .env("TERM", "xterm-256color");
    match launch.provider.as_str() {
        "codex" => {
            command
                .arg("resume")
                .arg(&launch.native_id)
                .env("CODEX_HOME", &launch.provider_home);
            if profile_lease.is_some()
                || native
                    .as_ref()
                    .is_some_and(|options| options.profile.is_some())
            {
                command.arg("--cd").arg(&launch.cwd);
            }
        }
        "claude" => {
            command
                .arg("--resume")
                .arg(&launch.native_id)
                .env("CLAUDE_CONFIG_DIR", &launch.provider_home);
        }
        _ => return Err("Unsupported managed provider".into()),
    }
    command
        .stdin(Stdio::from(slave.try_clone().map_err(|e| e.to_string())?))
        .stdout(Stdio::from(slave.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(slave));
    lease.configure_child(&mut command)?;
    if let Some(profile) = &profile_lease {
        profile.configure_child(&mut command)?;
    }
    if let Some(options) = &native {
        let provider = if launch.provider == "codex" {
            crate::agent_task::AgentProvider::Codex
        } else {
            crate::agent_task::AgentProvider::Claude
        };
        options
            .context
            .configure_authentication(&mut command, provider);
        options.execution_lease.configure_child(&mut command)?;
        if let Some(profile) = &options.profile {
            profile
                .configure_child_command(&mut command)
                .map_err(|e| e.to_string())?;
        }
        if let Some(lease) = &options.session_lease {
            lease.configure_child(&mut command)?;
        }
        if let Some(grant) = &options.grant {
            grant.configure_child(&mut command);
        }
    }
    let child = SupervisedChild::spawn_pty_session(&mut command)
        .map_err(|e| format!("Could not start {}: {e}", launch.provider))?;
    let pid = child.process_group_id() as u32;
    let Some(start) = super::ownership::supervised_process_start(&child) else {
        return Err("Could not record the owned provider process identity".into());
    };
    let snapshot = RuntimeSnapshot {
        cleanup_verified: false,
        id: launch.row.id.clone(),
        state: RuntimeState::Running,
        pid,
        process_start: start,
        epoch: uuid::Uuid::new_v4().to_string(),
        event_sequence: 0,
        cols,
        rows,
        exit_code: None,
        detail: "Persistent PTY owned by jterm Session Host".into(),
    };
    let record = LeaseRecord {
        id: launch.row.id.clone(),
        host_pid: std::process::id(),
        host_start: process_start(std::process::id())
            .ok_or("Cannot identify session host process")?,
        runtime: snapshot.clone(),
    };
    lease.write(&record)?;
    let view = Arc::new(Mutex::new(snapshot));
    let (control, rx) = bounded(128);
    let output = Arc::new(Mutex::new(RuntimeOutput::default()));
    let thread_output = output.clone();
    let thread_view = view.clone();
    std::thread::Builder::new()
        .name("jterm-managed-pty".into())
        .spawn(move || {
            let _profile_lease = profile_lease;
            let _native_options = native;
            runtime_loop(master, child, lease, record, rx, thread_view, thread_output)
        })
        .map_err(|e| e.to_string())?;
    Ok(RuntimeHandle {
        control,
        snapshot: view,
        output,
    })
}

fn dimensions(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(1, 400), rows.clamp(1, 120))
}

pub(super) fn open_pty(cols: u16, rows: u16) -> Result<(File, File), String> {
    let mut master = -1;
    let mut slave = -1;
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &winsize,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok((master, slave))
}

pub(super) fn runtime_loop(
    mut master: File,
    mut child: SupervisedChild,
    mut lease: Lease,
    mut record: LeaseRecord,
    controls: Receiver<Control>,
    view: Arc<Mutex<RuntimeSnapshot>>,
    output: Arc<Mutex<RuntimeOutput>>,
) {
    let mut screen = ScreenReplay::with_budget(
        record.runtime.cols as usize,
        record.runtime.rows as usize,
        128 * 1024,
    );
    let mut owner: Option<(String, Sender<Response>, Arc<AtomicBool>)> = None;
    let mut pending: VecDeque<u8> = VecDeque::new();
    let mut stopping: Option<Instant> = None;
    let mut bytes = [0u8; OUTPUT_CHUNK_BYTES];
    loop {
        if owner
            .as_ref()
            .is_some_and(|(_, _, closed)| closed.load(Ordering::Relaxed))
        {
            owner = None;
        }
        for control in controls.try_iter().take(64) {
            match control {
                Control::Attach {
                    client,
                    output,
                    closed,
                    cols,
                    rows,
                    ack,
                } => {
                    if owner.is_some() {
                        let _=ack.try_send(Err("This session is attached to another terminal. Detach that terminal with Ctrl+] before attaching here.".into()));
                        continue;
                    }
                    if stopping.is_some() || !pending.is_empty() {
                        let _=ack.try_send(Err("This session is stopping or draining previously accepted input. Retry after it settles.".into()));
                        continue;
                    }
                    resize(&master, &mut screen, &mut record.runtime, cols, rows);
                    record.runtime.state = if stopping.is_some() {
                        RuntimeState::Stopping
                    } else {
                        RuntimeState::Attached
                    };
                    let snapshot = match screen.snapshot_ansi_bounded(2 * 1024 * 1024) {
                        Ok(snapshot) => snapshot,
                        Err(error) => {
                            let _ = ack.try_send(Err(error));
                            continue;
                        }
                    };
                    let response = Response::Attached {
                        snapshot: record.runtime.clone(),
                        screen: snapshot,
                        truncated: true,
                    };
                    if output.try_send(response).is_err() {
                        let _ = ack.try_send(Err(
                            "Attachment closed before its snapshot was delivered".into(),
                        ));
                        continue;
                    }
                    owner = Some((client, output, closed));
                    let _ = ack.try_send(Ok(()));
                }
                Control::Input {
                    client,
                    epoch,
                    bytes,
                } if stopping.is_none()
                    && valid_owner(&owner, &client, &epoch, &record.runtime) =>
                {
                    if pending.len() + bytes.len() > MAX_INPUT {
                        detach_with_error(
                            &mut owner,
                            "Terminal input queue is full. Reattach after the provider responds.",
                        );
                        pending.clear();
                    } else {
                        pending.extend(bytes);
                    }
                }
                Control::Resize {
                    client,
                    epoch,
                    cols,
                    rows,
                } if valid_owner(&owner, &client, &epoch, &record.runtime) => {
                    resize(&master, &mut screen, &mut record.runtime, cols, rows)
                }
                Control::Detach { client, epoch }
                    if valid_owner(&owner, &client, &epoch, &record.runtime) =>
                {
                    owner = None;
                }
                Control::Stop if stopping.is_none() => {
                    pending.clear();
                    let _ = child.terminate_group();
                    stopping = Some(Instant::now());
                    record.runtime.state = RuntimeState::Stopping;
                }
                _ => {}
            }
        }
        if !pending.is_empty() {
            let contiguous = pending.make_contiguous();
            match master.write(contiguous) {
                Ok(n) => {
                    pending.drain(..n);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    pending.clear();
                    detach_with_error(&mut owner, "The provider closed its input terminal");
                }
            }
        }
        for _ in 0..32 {
            match master.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => {
                    screen.feed(&bytes[..n]);
                    {
                        let mut capture = output.lock();
                        capture.raw.extend(&bytes[..n]);
                        if capture.raw.len() > 128 * 1024 {
                            let excess = capture.raw.len() - 128 * 1024;
                            capture.raw.drain(..excess);
                            capture.truncated = true;
                        }
                    }
                    record.runtime.event_sequence = record.runtime.event_sequence.saturating_add(1);
                    if let Some((_, output, _)) = &owner {
                        if output
                            .try_send(Response::Output {
                                sequence: record.runtime.event_sequence,
                                bytes: bytes[..n].to_vec(),
                            })
                            .is_err()
                        {
                            detach_with_error(&mut owner,"Output consumer fell behind. Reattach to restore the current screen.");
                            pending.clear();
                        }
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted =>
                {
                    break
                }
                Err(_) => break,
            }
        }
        match child.root_has_exited() {
            Ok(exited)
                if exited
                    || stopping.is_some_and(|time| time.elapsed() > Duration::from_secs(2)) =>
            {
                // WNOWAIT retains the leader's PID until the group has been
                // killed. A TERM-ignoring tool cannot outlive early root exit.
                let group = child.process_group_id();
                let status = match child.reap_after_group_kill() {
                    Ok(status) => status,
                    Err(error) => {
                        record.runtime.state = RuntimeState::Failed;
                        record.runtime.detail = format!("Could not stop provider group: {error}");
                        detach_with_error(&mut owner, &record.runtime.detail);
                        output.lock().screen = screen
                            .snapshot_ansi_bounded(2 * 1024 * 1024)
                            .unwrap_or_default();
                        *view.lock() = record.runtime;
                        return;
                    }
                };
                let empty = wait_group_empty(group, Duration::from_secs(2));
                record.runtime.cleanup_verified = empty;
                record.runtime.state = RuntimeState::Exited;
                record.runtime.exit_code = Some(
                    status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
                );
                if !empty {
                    record.runtime.detail="Provider exited; descendant cleanup is still pending. Its inherited lease prevents a second writer.".into();
                }
                record.runtime.event_sequence = record.runtime.event_sequence.saturating_add(1);
                if let Some((_, output, _)) = owner.take() {
                    let _ = output.try_send(Response::Exited {
                        sequence: record.runtime.event_sequence,
                        code: record.runtime.exit_code,
                    });
                }
                let _ = lease.write(&record);
                output.lock().screen = screen
                    .snapshot_ansi_bounded(2 * 1024 * 1024)
                    .unwrap_or_default();
                *view.lock() = record.runtime;
                return;
            }
            Err(error) => {
                let _ = child.reap_after_group_kill();
                record.runtime.state = RuntimeState::Failed;
                record.runtime.detail = format!("Could not observe provider exit: {error}");
                detach_with_error(&mut owner, &record.runtime.detail);
                let _ = lease.write(&record);
                output.lock().screen = screen
                    .snapshot_ansi_bounded(2 * 1024 * 1024)
                    .unwrap_or_default();
                *view.lock() = record.runtime;
                return;
            }
            Ok(false) => {}
            Ok(true) => unreachable!(),
        }
        if stopping.is_none() {
            record.runtime.state = if owner.is_some() {
                RuntimeState::Attached
            } else {
                RuntimeState::Running
            };
        }
        *view.lock() = record.runtime.clone();
        let mut descriptor = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN | if pending.is_empty() { 0 } else { libc::POLLOUT },
            revents: 0,
        };
        unsafe {
            libc::poll(&mut descriptor, 1, 15);
        }
    }
}

fn valid_owner(
    owner: &Option<(String, Sender<Response>, Arc<AtomicBool>)>,
    client: &str,
    epoch: &str,
    snapshot: &RuntimeSnapshot,
) -> bool {
    owner
        .as_ref()
        .is_some_and(|(id, _, closed)| id == client && !closed.load(Ordering::Relaxed))
        && epoch == snapshot.epoch
}
fn detach_with_error(owner: &mut Option<(String, Sender<Response>, Arc<AtomicBool>)>, error: &str) {
    if let Some((_, output, _)) = owner.take() {
        let _ = output.try_send(Response::Error(error.into()));
    }
}
fn wait_group_empty(group: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if unsafe { libc::kill(-group, 0) } < 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}
fn resize(
    master: &File,
    screen: &mut ScreenReplay,
    snapshot: &mut RuntimeSnapshot,
    cols: u16,
    rows: u16,
) {
    let (cols, rows) = dimensions(cols, rows);
    if snapshot.cols == cols && snapshot.rows == rows {
        return;
    }
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &winsize) } == 0 {
        screen.resize(cols as usize, rows as usize);
        snapshot.cols = cols;
        snapshot.rows = rows;
    }
}

/// Provider CLI processes outside the host cannot be attached safely. Match
/// exact argv IDs and provider homes, or an open authoritative history file.
fn external_owner(launch: &ManagedLaunch) -> Option<u32> {
    let entries = std::fs::read_dir("/proc").ok()?;
    let canonical_source = launch.source.canonicalize().ok()?;
    let mut fd_budget = 4096;
    for entry in entries.take(32768).flatten() {
        let name = entry.file_name();
        let pid = name.to_str().and_then(|s| s.parse::<u32>().ok());
        let Some(pid) = pid else {
            continue;
        };
        if entry
            .metadata()
            .ok()
            .is_none_or(|m| m.uid() != unsafe { libc::geteuid() })
        {
            continue;
        }
        let mut raw = Vec::new();
        let Ok(file) = File::open(entry.path().join("cmdline")) else {
            continue;
        };
        if file.take(16385).read_to_end(&mut raw).is_err() || raw.len() > 16384 {
            continue;
        }
        let args: Vec<_> = raw
            .split(|b| *b == 0)
            .filter_map(|b| std::str::from_utf8(b).ok())
            .filter(|s| !s.is_empty())
            .collect();
        let cli = if args
            .first()
            .and_then(|s| std::path::Path::new(s).file_name())
            .and_then(|s| s.to_str())
            .is_some_and(|s| s == "node" || s == "nodejs")
        {
            1
        } else {
            0
        };
        let Some(program) = args.get(cli) else {
            continue;
        };
        let name = std::path::Path::new(program)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let provider_matches = if launch.provider == "codex" {
            matches!(name, "codex" | "codex.js")
        } else {
            name == "claude" || (name == "cli.js" && program.contains("@anthropic-ai/claude-code/"))
        };
        if !provider_matches {
            continue;
        }
        let flags = &args[cli + 1..];
        let exact = flags.windows(2).any(|pair| {
            pair[1] == launch.native_id
                && (pair[0] == "resume"
                    || pair[0] == "--resume"
                    || pair[0] == "-r"
                    || pair[0] == "--session-id")
        });
        if exact {
            let mut env = Vec::new();
            let home = File::open(entry.path().join("environ"))
                .ok()
                .and_then(|f| f.take(128 * 1024).read_to_end(&mut env).ok())
                .and_then(|_| {
                    let variable = if launch.provider == "codex" {
                        "CODEX_HOME="
                    } else {
                        "CLAUDE_CONFIG_DIR="
                    };
                    let envs: Vec<_> = env
                        .split(|b| *b == 0)
                        .filter_map(|b| std::str::from_utf8(b).ok())
                        .collect();
                    envs.iter()
                        .find_map(|v| v.strip_prefix(variable).map(std::path::PathBuf::from))
                        .or_else(|| {
                            envs.iter().find_map(|v| v.strip_prefix("HOME=")).map(|h| {
                                std::path::PathBuf::from(h).join(if launch.provider == "codex" {
                                    ".codex"
                                } else {
                                    ".claude"
                                })
                            })
                        })
                });
            if home.is_none_or(|home| home.canonicalize().unwrap_or(home) == launch.provider_home) {
                return Some(pid);
            }
        }
        if fd_budget > 0 {
            if let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) {
                for fd in fds.take(256).flatten() {
                    if fd_budget == 0 {
                        break;
                    }
                    fd_budget -= 1;
                    if std::fs::read_link(fd.path()).ok().as_ref() == Some(&canonical_source) {
                        return Some(pid);
                    }
                }
            }
        }
    }
    None
}
