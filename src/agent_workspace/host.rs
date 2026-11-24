use super::model::*;
use super::ownership::*;
use super::protocol::*;
use super::store::Store;
use super::PROTOCOL_VERSION;
use crate::agent_sessions::{AgentSession, SessionProvider, SessionStatus};
use crate::native_pty_launch::{launch_hosted_pty, PtyLaunchError};
use crate::screen_replay::ScreenReplay;
use crate::supervised::SupervisedChild;
use crossbeam_channel::{bounded, Receiver, Sender};
use jterm_runtime::{
    exit_status::ExitTail,
    runtime_wake::WakeSink,
    session_actor::{HostedPty, SessionActor, SessionEvent, SplitWriteLimits},
};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_RUNTIMES: usize = 16;
const MAX_CLIENTS: usize = 64;
const MAX_INPUT: usize = 64 * 1024;
const HOSTED_INPUT_ENTRIES: usize = 128;
const HOSTED_REPLY_ENTRIES: usize = 16;
const STOP_GRACE: Duration = Duration::from_secs(2);
const RUNTIME_OUTPUT_BYTES: usize = 128 * 1024;
const RUNTIME_SCREEN_BYTES: usize = 2 * 1024 * 1024;

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
        kind: InputKind,
        processed: Sender<()>,
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
                    let command = match native.handle(command, owner.clone(), &mut store, |row| {
                        project_runtime_row(row, &runtimes, &paths)
                    }) {
                        super::api_host::Disposition::Handled(reply) => return *reply,
                        super::api_host::Disposition::Other(command) => command,
                    };
                    match command {
                        WorkspaceCommand::DeleteCliSession { id, session } => {
                            if session_id(&session)? != id {
                                return Err("The provider session source changed. Refresh before deleting it.".into());
                            }
                            if runtimes
                                .get(&id)
                                .is_some_and(|r| r.snapshot.lock().state.is_live())
                            {
                                return Err(
                                    "Stop the running CLI session before deleting it.".into()
                                );
                            }
                            let _lease = Lease::acquire(&paths.lease(&id))?;
                            let home = session
                                .provider_home()
                                .ok_or("Cannot identify the original provider home")?;
                            if external_owner(
                                &session.provider.label().to_ascii_lowercase(),
                                &session.id,
                                &home,
                                &session.source,
                            )
                            .is_some()
                            {
                                return Err(
                                    "Stop the running provider session before deleting it.".into(),
                                );
                            }
                            store.delete_cli_session(&id)
                        }
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
                    if closed.load(Ordering::Relaxed) {
                        return Err("Attachment closed before Session Host admitted it".into());
                    }
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
                    if store.cli_session_deleted(&id)? {
                        return Err("This session was deleted from Agent Workspace.".into());
                    }
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
                    if let Some(pid) = external_owner(
                        &launch.provider,
                        &launch.native_id,
                        &launch.provider_home,
                        &launch.source,
                    ) {
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
        attach_exited(&runtime, snapshot, &output)?;
        return Ok(runtime);
    }
    let (ack, rx) = bounded(1);
    if runtime
        .control
        .try_send(Control::Attach {
            client,
            output: output.clone(),
            closed,
            cols,
            rows,
            ack,
        })
        .is_err()
    {
        let snapshot = runtime.snapshot.lock().clone();
        if snapshot.state.is_live() {
            return Err("The PTY is registered but its attachment queue is busy".into());
        }
        attach_exited(&runtime, snapshot, &output)?;
    } else {
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(result) => result?,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                let snapshot = runtime.snapshot.lock().clone();
                if snapshot.state.is_live() {
                    return Err("The PTY is registered but its attachment closed".into());
                }
                attach_exited(&runtime, snapshot, &output)?;
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                return Err("The PTY is registered but its attachment timed out".into());
            }
        }
    }
    Ok(runtime)
}

fn attach_exited(
    runtime: &RuntimeHandle,
    snapshot: RuntimeSnapshot,
    output: &Sender<Response>,
) -> Result<(), String> {
    let (sequence, code) = (snapshot.event_sequence, snapshot.exit_code);
    output
        .try_send(Response::Attached {
            snapshot,
            screen: runtime.output.lock().screen.clone(),
            truncated: true,
        })
        .map_err(|_| "Attachment closed")?;
    output
        .try_send(Response::Exited { sequence, code })
        .map_err(|_| "Attachment closed".into())
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
            if host
                .try_send(HostRequest::Attach(
                    target,
                    client.clone(),
                    output,
                    writer_closed.clone(),
                    reply,
                ))
                .is_err()
            {
                return write_frame(
                    &mut stream,
                    &Response::Error("Session Host attachment queue is busy".into()),
                );
            }
            let runtime = match rx.recv_timeout(Duration::from_secs(8)) {
                Ok(Ok(runtime)) => runtime,
                Ok(Err(error)) => return write_frame(&mut stream, &Response::Error(error)),
                Err(_) => {
                    writer_closed.store(true, Ordering::Relaxed);
                    let message = "Session Host did not confirm the attachment. The PTY may already be running.";
                    return write_frame(&mut stream, &Response::Error(message.into()));
                }
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
            let (processed, admission) = bounded(1);
            while let Ok(Some(request)) =
                read_frame_bounded::<Request>(&mut reader, MAX_ATTACHMENT_FRAME_BYTES)
            {
                let control = match request {
                    Request::Input {
                        client: c,
                        epoch: e,
                        bytes,
                        kind,
                    } if c == client
                        && e == epoch
                        && !bytes.is_empty()
                        && bytes.len() <= kind.limit() =>
                    {
                        Control::Input {
                            client: c,
                            epoch: e,
                            bytes,
                            kind,
                            processed: processed.clone(),
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
                let awaits_admission = matches!(&control, Control::Input { .. });
                if runtime.control.try_send(control).is_err()
                    || (awaits_admission && admission.recv_timeout(Duration::from_secs(3)).is_err())
                {
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
    if native.is_none()
        && launch.provider == "codex"
        && launch
            .provider_home
            .join("jterm-native-profile.json")
            .is_file()
    {
        let _profile_lease = NativeSessionLease::acquire_at(
            &paths.state,
            SessionProvider::Codex,
            &launch.provider_home,
            "native-profile",
        )?;
        return Err("Open the native task and choose Continue in Terminal to acquire a fresh access grant for this exact thread".into());
    }
    let lease = match native
        .as_mut()
        .and_then(|options| options.session_lease.take())
    {
        Some(lease) => lease.into_lease(),
        None => Lease::acquire(&paths.lease(&launch.row.id))?,
    };
    let (cols, rows) = dimensions(launch.cols, launch.rows);
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
            if native
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
    let mut inherited = vec![lease.as_fd()];
    if let Some(options) = &native {
        let provider = if launch.provider == "codex" {
            crate::agent_task::AgentProvider::Codex
        } else {
            crate::agent_task::AgentProvider::Claude
        };
        options
            .context
            .configure_authentication(&mut command, provider);
        inherited.push(options.execution_lease.as_ref().as_fd());
        if let Some(profile) = &options.profile {
            inherited.extend(profile.inherited_descriptors());
        }
        if let Some(grant) = &options.grant {
            grant.configure_environment(&mut command);
            inherited.extend(grant.inherited_descriptors());
        }
    }
    let (master, child) = launch_hosted_pty(&command, None, rows, cols, &inherited)
        .map_err(|e| format!("Could not start {}: {e}", launch.provider))?;
    register_pty(
        launch.row.id.clone(),
        (cols, rows),
        "jterm-managed-pty",
        "Persistent PTY owned by jterm Session Host",
        RuntimePty {
            master,
            child,
            lease,
            guard: native,
        },
    )
    .map_err(|e| e.to_string())
}

pub(super) struct RuntimePty<G> {
    pub master: OwnedFd,
    pub child: SupervisedChild,
    pub lease: Lease,
    // Keep supplementary leases and grants alive through child cleanup,
    // including failed registration before the worker has taken ownership.
    pub guard: G,
}

struct RegisteredPty<G> {
    actor: SessionActor<HostedPty>,
    lease: Lease,
    guard: G,
}

pub(super) fn register_pty<G: Send + 'static>(
    id: ConversationId,
    (cols, rows): (u16, u16),
    thread_name: &str,
    detail: &str,
    pty: RuntimePty<G>,
) -> Result<RuntimeHandle, PtyLaunchError> {
    let pid = pty.child.process_group_id() as u32;
    let Some(start) = supervised_process_start(&pty.child) else {
        return Err(PtyLaunchError::ExecConfirmed(io::Error::other(
            "cannot record the owned PTY process identity",
        )));
    };
    let host_pid = std::process::id();
    let host_start = process_start(host_pid).ok_or_else(|| {
        PtyLaunchError::ExecConfirmed(io::Error::other("cannot identify Session Host process"))
    })?;
    let snapshot = RuntimeSnapshot {
        cleanup_verified: false,
        id: id.clone(),
        state: RuntimeState::Running,
        pid,
        process_start: start,
        epoch: uuid::Uuid::new_v4().to_string(),
        event_sequence: 0,
        cols,
        rows,
        exit_code: None,
        detail: detail.into(),
    };
    let record = LeaseRecord {
        id,
        host_pid,
        host_start,
        runtime: snapshot.clone(),
    };
    let mut pty = RegisteredPty {
        actor: SessionActor::start_hosted(
            pty.master,
            pty.child,
            SplitWriteLimits {
                user_bytes: MAX_INPUT,
                response_bytes: REPLY_CHUNK_BYTES,
                entries: HOSTED_INPUT_ENTRIES,
                coalesce_bytes: 0,
            },
            HOSTED_REPLY_ENTRIES,
            OUTPUT_CHUNK_BYTES,
            WakeSink::new(|| {}),
        )
        .map_err(|error| PtyLaunchError::ExecConfirmed(io::Error::other(error)))?,
        lease: pty.lease,
        guard: pty.guard,
    };
    let view = Arc::new(Mutex::new(snapshot));
    let (control, rx) = bounded(128);
    let (registered, registration) = bounded::<(RegisteredPty<G>, LeaseRecord)>(1);
    let output = Arc::new(Mutex::new(RuntimeOutput::default()));
    let thread_output = output.clone();
    let thread_view = view.clone();
    std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            if let Ok((
                RegisteredPty {
                    actor,
                    lease,
                    guard: _guard,
                },
                record,
            )) = registration.recv()
            {
                runtime_loop(actor, lease, record, rx, thread_view, thread_output);
            }
        })
        .map_err(|error| {
            PtyLaunchError::ExecConfirmed(io::Error::other(format!(
                "cannot start PTY worker: {error}"
            )))
        })?;
    pty.lease.write(&record).map_err(|error| {
        PtyLaunchError::ExecConfirmed(io::Error::other(format!(
            "cannot register PTY lease: {error}"
        )))
    })?;
    registered.send((pty, record)).map_err(|_| {
        PtyLaunchError::ExecConfirmed(io::Error::other("PTY worker stopped during registration"))
    })?;
    Ok(RuntimeHandle {
        control,
        snapshot: view,
        output,
    })
}

fn dimensions(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(1, 400), rows.clamp(1, 120))
}

type Attachment = (String, Sender<Response>, Arc<AtomicBool>);

fn publish_output(
    bytes: &[u8],
    screen: &mut ScreenReplay,
    output: &Mutex<RuntimeOutput>,
    owner: &mut Option<Attachment>,
    sequence: &mut u64,
) {
    screen.feed(bytes);
    {
        let mut capture = output.lock();
        capture.raw.extend(bytes);
        if capture.raw.len() > RUNTIME_OUTPUT_BYTES {
            let excess = capture.raw.len() - RUNTIME_OUTPUT_BYTES;
            capture.raw.drain(..excess);
            capture.truncated = true;
        }
    }
    *sequence = sequence.saturating_add(1);
    if let Some((_, attached, _)) = owner {
        if attached
            .try_send(Response::Output {
                sequence: *sequence,
                bytes: bytes.to_vec(),
            })
            .is_err()
        {
            detach_with_error(
                owner,
                "Output consumer fell behind. Reattach to restore the current screen.",
            );
        }
    }
}

fn runtime_loop(
    actor: SessionActor<HostedPty>,
    mut lease: Lease,
    mut record: LeaseRecord,
    controls: Receiver<Control>,
    view: Arc<Mutex<RuntimeSnapshot>>,
    output: Arc<Mutex<RuntimeOutput>>,
) {
    let mut screen = ScreenReplay::with_budget(
        record.runtime.cols as usize,
        record.runtime.rows as usize,
        RUNTIME_OUTPUT_BYTES,
    );
    let mut owner: Option<Attachment> = None;
    let mut kill_at: Option<Instant> = None;
    let mut next_control = None;
    let mut next_event = None;
    let mut tail = ExitTail::Complete;

    let exit = 'running: loop {
        if owner
            .as_ref()
            .is_some_and(|(_, _, closed)| closed.load(Ordering::Relaxed))
        {
            owner = None;
        }
        for control in next_control
            .take()
            .into_iter()
            .chain(controls.try_iter())
            .take(64)
        {
            match control {
                Control::Attach {
                    client,
                    output,
                    closed,
                    cols,
                    rows,
                    ack,
                } => {
                    if closed.load(Ordering::Relaxed) {
                        let _ = ack
                            .try_send(Err("Attachment closed before the PTY could attach".into()));
                        continue;
                    }
                    if owner.is_some() {
                        let _ = ack.try_send(Err("This session is attached to another terminal. Detach that terminal with Ctrl+] before attaching here.".into()));
                        continue;
                    }
                    if record.runtime.state == RuntimeState::Stopping
                        || actor.input_drained() != Ok(true)
                    {
                        let _ = ack.try_send(Err("The session is stopping or its input queue is unavailable. Retry after it settles.".into()));
                        continue;
                    }
                    resize(&actor, &mut screen, &mut record.runtime, cols, rows);
                    let replay = match screen.snapshot_ansi_bounded(RUNTIME_SCREEN_BYTES) {
                        Ok(replay) => replay,
                        Err(error) => {
                            let _ = ack.try_send(Err(error));
                            continue;
                        }
                    };
                    let mut snapshot = record.runtime.clone();
                    snapshot.state = RuntimeState::Attached;
                    let response = Response::Attached {
                        snapshot,
                        screen: replay,
                        truncated: true,
                    };
                    if output.try_send(response).is_err() {
                        let _ = ack.try_send(Err(
                            "Attachment closed before its snapshot was delivered".into(),
                        ));
                        continue;
                    }
                    if ack.try_send(Ok(())).is_ok() {
                        owner = Some((client, output, closed));
                        record.runtime.state = RuntimeState::Attached;
                    }
                }
                Control::Input {
                    client,
                    epoch,
                    bytes,
                    kind,
                    processed,
                } => {
                    if record.runtime.state != RuntimeState::Stopping
                        && valid_owner(&owner, &client, &epoch, &record.runtime)
                    {
                        let result = match kind {
                            InputKind::User => actor.write_sender().try_send(bytes),
                            InputKind::Reply => actor.write_response(bytes),
                            InputKind::Paste => actor.write_paste(bytes),
                        };
                        if let Err(error) = result {
                            // Keep earlier accepted writes and their FIFO order.
                            detach_with_error(
                                &mut owner,
                                &format!("Workspace input rejected. {error}"),
                            );
                        }
                    }
                    // At most one decoded input can wait outside the actor's
                    // byte budget per attachment, including under socket load.
                    let _ = processed.try_send(());
                }
                Control::Resize {
                    client,
                    epoch,
                    cols,
                    rows,
                } if valid_owner(&owner, &client, &epoch, &record.runtime) => {
                    resize(&actor, &mut screen, &mut record.runtime, cols, rows);
                }
                Control::Detach { client, epoch }
                    if valid_owner(&owner, &client, &epoch, &record.runtime) =>
                {
                    owner = None;
                }
                Control::Stop if record.runtime.state != RuntimeState::Stopping => {
                    if let Err(error) = actor.signal_host_stop(false) {
                        log::warn!("Could not signal provider to stop: {error}");
                    }
                    kill_at = Some(Instant::now() + STOP_GRACE);
                    record.runtime.state = RuntimeState::Stopping;
                }
                _ => {}
            }
        }

        for event in next_event
            .take()
            .into_iter()
            .chain(actor.events().try_iter())
            .take(32)
        {
            match event {
                SessionEvent::Output(batch) => {
                    for bytes in batch.into_bytes().chunks(OUTPUT_CHUNK_BYTES) {
                        publish_output(
                            bytes,
                            &mut screen,
                            &output,
                            &mut owner,
                            &mut record.runtime.event_sequence,
                        );
                    }
                }
                SessionEvent::ExitTail(status) => tail = status,
                SessionEvent::Exit(code) => break 'running Ok(code),
                SessionEvent::Error(error) => {
                    if let Some(exit_error) = actor.hosted_exit_error() {
                        break 'running Err(exit_error);
                    }
                    record.runtime.detail = error.clone();
                    detach_with_error(&mut owner, &error);
                }
            }
        }

        if kill_at.is_some_and(|deadline| Instant::now() >= deadline) {
            kill_at = None;
            if let Err(error) = actor.signal_host_stop(true) {
                log::warn!("Could not escalate provider stop: {error}");
            }
        }
        if record.runtime.state != RuntimeState::Stopping {
            record.runtime.state = if owner.is_some() {
                RuntimeState::Attached
            } else {
                RuntimeState::Running
            };
        }
        *view.lock() = record.runtime.clone();
        let timeout = kill_at.map_or(Duration::from_secs(60), |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        crossbeam_channel::select! {
            recv(controls) -> message => match message {
                Ok(control) => next_control = Some(control),
                Err(_) => break Err("Session Host released the PTY coordinator".into()),
            },
            recv(actor.events()) -> message => match message {
                Ok(event) => next_event = Some(event),
                Err(_) => break Err("Shared PTY actor stopped before publishing exit".into()),
            },
            default(timeout) => {}
        }
    };

    match exit {
        Ok(code) => {
            record.runtime.cleanup_verified = actor.hosted_cleanup_verified();
            record.runtime.state = RuntimeState::Exited;
            record.runtime.exit_code = Some(code);
            if !record.runtime.cleanup_verified {
                record.runtime.detail.push_str(" Descendant cleanup is still pending. Its inherited lease prevents a second writer.");
            }
            if let Some(notice) = tail.notice() {
                output.lock().truncated = true;
                record.runtime.detail.push(' ');
                record.runtime.detail.push_str(notice);
            }
            record.runtime.event_sequence = record.runtime.event_sequence.saturating_add(1);
        }
        Err(error) => {
            record.runtime.state = RuntimeState::Failed;
            record.runtime.detail = error;
        }
    }
    // Synchronous fallback cleanup keeps the leases through the owned wait.
    drop(actor);
    let event = if record.runtime.state == RuntimeState::Exited {
        Response::Exited {
            sequence: record.runtime.event_sequence,
            code: record.runtime.exit_code,
        }
    } else {
        Response::Error(record.runtime.detail.clone())
    };
    let _ = lease.write(&record);
    output.lock().screen = screen
        .snapshot_ansi_bounded(RUNTIME_SCREEN_BYTES)
        .unwrap_or_default();
    *view.lock() = record.runtime;
    if let Some((_, output, _)) = owner.take() {
        let _ = output.try_send(event);
    }
}

fn valid_owner(
    owner: &Option<Attachment>,
    client: &str,
    epoch: &str,
    snapshot: &RuntimeSnapshot,
) -> bool {
    owner
        .as_ref()
        .is_some_and(|(id, _, closed)| id == client && !closed.load(Ordering::Relaxed))
        && epoch == snapshot.epoch
}
fn detach_with_error(owner: &mut Option<Attachment>, error: &str) {
    if let Some((_, output, _)) = owner.take() {
        let _ = output.try_send(Response::Error(error.into()));
    }
}

fn resize(
    actor: &SessionActor<HostedPty>,
    screen: &mut ScreenReplay,
    snapshot: &mut RuntimeSnapshot,
    cols: u16,
    rows: u16,
) {
    let (cols, rows) = dimensions(cols, rows);
    if snapshot.cols == cols && snapshot.rows == rows {
        return;
    }
    if actor.resize(cols as usize, rows as usize).is_ok() {
        screen.resize(cols as usize, rows as usize);
        snapshot.cols = cols;
        snapshot.rows = rows;
    }
}

/// Provider CLI processes outside the host cannot be attached safely. Match
/// exact argv IDs and provider homes, or an open authoritative history file.
fn external_owner(
    provider: &str,
    native_id: &str,
    provider_home: &std::path::Path,
    source: &std::path::Path,
) -> Option<u32> {
    let entries = std::fs::read_dir("/proc").ok()?;
    let canonical_source = source.canonicalize().ok();
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
        let provider_matches = if provider == "codex" {
            matches!(name, "codex" | "codex.js")
        } else {
            name == "claude" || (name == "cli.js" && program.contains("@anthropic-ai/claude-code/"))
        };
        if !provider_matches {
            continue;
        }
        let flags = &args[cli + 1..];
        let exact = flags.windows(2).any(|pair| {
            pair[1] == native_id
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
                    let variable = if provider == "codex" {
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
                                std::path::PathBuf::from(h).join(if provider == "codex" {
                                    ".codex"
                                } else {
                                    ".claude"
                                })
                            })
                        })
                });
            if home.is_none_or(|home| home.canonicalize().unwrap_or(home) == provider_home) {
                return Some(pid);
            }
        }
        if fd_budget > 0 && canonical_source.is_some() {
            if let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) {
                for fd in fds.take(256).flatten() {
                    if fd_budget == 0 {
                        break;
                    }
                    fd_budget -= 1;
                    if std::fs::read_link(fd.path()).ok() == canonical_source {
                        return Some(pid);
                    }
                }
            }
        }
    }
    None
}
