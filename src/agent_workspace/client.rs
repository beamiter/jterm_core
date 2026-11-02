use super::host::{peer_is_same_user, run};
use super::model::*;
use super::ownership::*;
use super::protocol::*;
use super::PROTOCOL_VERSION;
use crate::agent_sessions::{AgentSession, SessionLaunch};
use crossbeam_channel::{bounded, Receiver, Sender};
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const HOST_FLAG: &str = "--jterm-session-host";
const ATTACH_FLAG: &str = "--jterm-session-attach";
const OWNED_ATTACH_FLAG: &str = "--jterm-owned-attach";

/// A bounded asynchronous interface. Constructing this object performs no
/// filesystem/socket access and starts no daemon. Commands are handled on a
/// worker; poll from the toolkit's ordinary idle/update callback.
pub struct WorkspaceClient {
    commands: Sender<(u64, WorkspaceCommand)>,
    events: Receiver<WorkspaceEvent>,
    next: Arc<AtomicU64>,
}

impl Default for WorkspaceClient {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Debug for WorkspaceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceClient")
            .field("pending", &self.commands.len())
            .field("ready", &self.events.len())
            .finish()
    }
}

impl WorkspaceClient {
    pub fn new() -> Self {
        Self::worker(None)
    }
    /// Explicit endpoint/executable for packaged hosts and integration tests.
    /// Like new(), this starts no process until a command is sent.
    pub fn with_host_executable(
        state_dir: impl AsRef<std::path::Path>,
        executable: impl AsRef<std::path::Path>,
    ) -> Self {
        Self::worker(Some((
            state_dir.as_ref().to_path_buf(),
            executable.as_ref().to_path_buf(),
        )))
    }
    /// Connect only to an already-running host in an isolated state directory.
    /// Never tries to launch the test harness as a frontend executable.
    pub fn for_state_dir(state_dir: impl AsRef<std::path::Path>) -> Self {
        Self::worker(Some((
            state_dir.as_ref().to_path_buf(),
            std::path::PathBuf::new(),
        )))
    }
    fn worker(endpoint: Option<(std::path::PathBuf, std::path::PathBuf)>) -> Self {
        let (commands, rx) = bounded::<(u64, WorkspaceCommand)>(32);
        let (tx, events) = bounded(64);
        std::thread::Builder::new()
            .name("jterm-workspace-client".into())
            .spawn(move || {
                for (request_id, command) in rx {
                    let result = (|| {
                        let (paths, exe, start) = match &endpoint {
                            Some((state, exe)) => (
                                Paths::at(state.clone())?,
                                Some(exe.as_path()),
                                !exe.as_os_str().is_empty(),
                            ),
                            None => (Paths::discover()?, None, true),
                        };
                        let stream = connect(&paths, start, exe)?;
                        command_request(stream, command)
                    })();
                    // Backpressure is bounded and happens on this worker, never
                    // the GUI thread. Dropping a frontend disconnects the queue.
                    if tx
                        .send(WorkspaceEvent::Reply { request_id, result })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("spawn workspace client worker");
        Self {
            commands,
            events,
            next: Arc::new(AtomicU64::new(1)),
        }
    }
    pub fn send(&self, command: WorkspaceCommand) -> Result<u64, String> {
        let request_id = self.next.fetch_add(1, Ordering::Relaxed);
        self.commands.try_send((request_id, command)).map_err(|_| {
            "Workspace queue is full or unavailable; retry after pending operations finish"
                .to_owned()
        })?;
        Ok(request_id)
    }
    pub fn poll(&self) -> Vec<WorkspaceEvent> {
        self.events.try_iter().take(64).collect()
    }
}

pub(super) fn command_request(
    mut stream: UnixStream,
    command: WorkspaceCommand,
) -> Result<WorkspaceReply, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    write_frame(
        &mut stream,
        &Request::Command {
            version: PROTOCOL_VERSION,
            command,
        },
    )?;
    match read_frame::<Response>(&mut BufReader::new(stream))?
        .ok_or("Session host closed its response")?
    {
        Response::Reply(result) => result,
        Response::Error(error) => Err(error),
        _ => Err("Unexpected workspace response".into()),
    }
}

/// Build an argv launch for the frontend's ordinary PTY pane. This function
/// validates the source/cwd but never connects, starts a daemon or waits for
/// a provider. Call it where existing resume preflight has already passed.
pub fn managed_launch(session: &AgentSession) -> Result<SessionLaunch, String> {
    let launch = descriptor(session)?;
    let program = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .into_os_string()
        .into_string()
        .map_err(|_| "Frontend executable path is not UTF-8")?;
    Ok(SessionLaunch {
        program,
        args: vec![
            ATTACH_FLAG.into(),
            serde_json::to_string(&launch).map_err(|e| e.to_string())?,
        ],
        cwd: session.cwd.clone(),
    })
}

/// Build a view attachment to an already admitted host runtime. The server
/// never starts a process from this descriptor, including after host restart.
pub fn owned_terminal_launch(
    terminal: &super::native::OwnedTerminalDescriptor,
) -> Result<SessionLaunch, String> {
    if !terminal.cwd.is_absolute()
        || uuid::Uuid::parse_str(&terminal.epoch).is_err()
        || !terminal.id.0.starts_with("jterm:validation:")
    {
        return Err("Invalid owned terminal identity".into());
    }
    let program = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .into_os_string()
        .into_string()
        .map_err(|_| "Frontend executable path is not UTF-8")?;
    Ok(SessionLaunch {
        program,
        args: vec![
            OWNED_ATTACH_FLAG.into(),
            serde_json::to_string(terminal).map_err(|e| e.to_string())?,
        ],
        cwd: if terminal.cwd.is_dir() {
            terminal.cwd.clone()
        } else {
            std::path::PathBuf::from("/")
        },
    })
}

/// A cheap ownership hint for an existing scanner conflict gate. The host
/// still enforces the kernel lease and input owner at attachment time.
pub fn managed_session_running(session: &AgentSession) -> bool {
    let Ok(id) = session_id(session) else {
        return false;
    };
    let Ok(paths) = Paths::locate() else {
        return false;
    };
    lease_is_held(&paths.lease(&id))
        && read_lease(&paths.lease(&id)).is_some_and(|record| {
            record.id == id
                && record.host_pid != 0
                && process_matches(record.host_pid, &record.host_start)
                && process_matches(record.runtime.pid, &record.runtime.process_start)
        })
}

/// Run this before GTK/egui/iced initialization and ordinary CLI parsing.
/// Returning None means this is a normal frontend invocation.
pub fn run_embedded_if_requested() -> Option<i32> {
    let mut args = std::env::args_os();
    let _ = args.next();
    let flag = args.next()?;
    if flag == super::api::API_WORKER_FLAG {
        if args.next().is_some() {
            return Some(2);
        }
        return Some(if super::api::run_worker().is_ok() {
            0
        } else {
            1
        });
    }
    if flag == HOST_FLAG {
        if args.next().is_some() {
            eprintln!("Session host does not accept additional arguments");
            return Some(2);
        }
        return Some(match Paths::discover().and_then(run) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("Session host: {error}");
                1
            }
        });
    }
    if flag == OWNED_ATTACH_FLAG {
        let result = (|| {
            let payload = args
                .next()
                .and_then(|s| s.into_string().ok())
                .ok_or("Missing owned terminal descriptor")?;
            if payload.len() > 65536 || args.next().is_some() {
                return Err("Invalid owned terminal arguments".into());
            }
            let terminal: super::native::OwnedTerminalDescriptor =
                serde_json::from_str(&payload).map_err(|e| e.to_string())?;
            owned_terminal_launch(&terminal)?;
            attach_owned_terminal(terminal)
        })();
        return Some(match result {
            Ok(code) => code,
            Err(error) => {
                eprintln!("\r\nOwned terminal could not be attached: {error}\r");
                1
            }
        });
    }
    if flag == ATTACH_FLAG {
        let payload = args.next().and_then(|s| s.into_string().ok());
        if args.next().is_some() {
            eprintln!("Invalid session attachment arguments");
            return Some(2);
        }
        let result = (|| {
            let payload = payload.ok_or("Missing session attachment descriptor")?;
            if payload.len() > 65536 {
                return Err("Session attachment descriptor is too large".into());
            }
            let launch: ManagedLaunch = serde_json::from_str(&payload)
                .map_err(|e| format!("Invalid session attachment: {e}"))?;
            attach_terminal(launch)
        })();
        return Some(match result {
            Ok(code) => code,
            Err(error) => {
                eprintln!("\r\nSession could not be attached: {error}\r");
                1
            }
        });
    }
    None
}

pub(super) fn connect(
    paths: &Paths,
    start: bool,
    executable: Option<&std::path::Path>,
) -> Result<UnixStream, String> {
    if let Ok(stream) = UnixStream::connect(&paths.socket) {
        if peer_is_same_user(&stream) {
            return Ok(stream);
        }
        return Err("Session host belongs to another user".into());
    }
    if !start {
        return Err("Session host is not running".into());
    }
    let exe = match executable {
        Some(exe) => exe.to_path_buf(),
        None => std::env::current_exe().map_err(|e| e.to_string())?,
    };
    let mut command = Command::new(exe);
    command
        .arg(HOST_FLAG)
        .env("JTERM_WORKSPACE_STATE_DIR", &paths.state)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("Could not start the session host: {e}"))?;
    // Reap the host if it exits while this GUI remains open. The thread does
    // not own provider PTYs; dropping the GUI cannot kill the host.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(stream) = UnixStream::connect(&paths.socket) {
            if peer_is_same_user(&stream) {
                return Ok(stream);
            }
            return Err("Session host peer verification failed".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err("Session host did not become ready. Check that this frontend handles --jterm-session-host before initializing its GUI and that the workspace directory is private.".into())
}

struct RawTerminal {
    original: libc::termios,
}
impl RawTerminal {
    fn enter() -> Result<Self, String> {
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut original) != 0 {
                return Err("A real terminal is required to attach a provider session".into());
            }
            let mut raw = original;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            Ok(Self { original })
        }
    }
}
impl Drop for RawTerminal {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.original);
        }
    }
}

fn terminal_size() -> (u16, u16) {
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(0, libc::TIOCGWINSZ, &mut size) == 0 && size.ws_col > 0 && size.ws_row > 0 {
            (size.ws_col.clamp(1, 400), size.ws_row.clamp(1, 120))
        } else {
            (120, 36)
        }
    }
}

fn attach_terminal(mut launch: ManagedLaunch) -> Result<i32, String> {
    let (cols, rows) = terminal_size();
    launch.cols = cols;
    launch.rows = rows;
    let client = uuid::Uuid::new_v4().to_string();
    let request = Request::Attach {
        version: PROTOCOL_VERSION,
        launch: Box::new(launch),
        client: client.clone(),
    };
    attach_request(request, client, cols, rows, true, false)
}
fn attach_owned_terminal(terminal: super::native::OwnedTerminalDescriptor) -> Result<i32, String> {
    let (cols, rows) = terminal_size();
    let client = uuid::Uuid::new_v4().to_string();
    let request = Request::AttachOwned {
        version: PROTOCOL_VERSION,
        terminal,
        client: client.clone(),
        cols,
        rows,
    };
    attach_request(request, client, cols, rows, false, true)
}
fn attach_request(
    request: Request,
    client: String,
    cols: u16,
    rows: u16,
    start: bool,
    hold_exited_view: bool,
) -> Result<i32, String> {
    let paths = Paths::discover()?;
    let mut stream = connect(&paths, start, None)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    write_frame(&mut stream, &request)?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let (snapshot, screen) =
        match read_frame::<Response>(&mut reader)?.ok_or("Session host closed the attachment")? {
            Response::Attached {
                snapshot, screen, ..
            } => (snapshot, screen),
            Response::Error(error) => return Err(error),
            _ => return Err("Unexpected attachment response".into()),
        };
    let raw = RawTerminal::enter()?;
    let mut stdout = std::io::stdout();
    stdout
        .write_all(&screen)
        .and_then(|_| stdout.flush())
        .map_err(|e| e.to_string())?;
    let epoch = snapshot.epoch;
    let (events, rx) = bounded(64);
    reader
        .get_mut()
        .set_read_timeout(None)
        .map_err(|e| e.to_string())?;
    let output_thread = std::thread::spawn(move || loop {
        match read_frame::<Response>(&mut reader) {
            Ok(Some(event)) => {
                let done = matches!(event, Response::Exited { .. } | Response::Error(_));
                if events.send(event).is_err() || done {
                    break;
                }
            }
            Ok(None) => {
                let _ = events.send(Response::Error("Session host connection closed".into()));
                break;
            }
            Err(error) => {
                let _ = events.send(Response::Error(error));
                break;
            }
        }
    });
    let mut sequence = snapshot.event_sequence;
    let mut size = (cols, rows);
    let mut input = [0u8; OUTPUT_CHUNK_BYTES];
    let mut result = Ok(0);
    let mut exited = false;
    'attach: loop {
        for event in rx.try_iter().take(64) {
            match event {
                Response::Output {
                    sequence: next,
                    bytes,
                } => {
                    if next != sequence.saturating_add(1) {
                        result = Err(
                            "Terminal event sequence has a gap; reattach to restore the screen"
                                .into(),
                        );
                        break 'attach;
                    }
                    sequence = next;
                    if let Err(error) = stdout.write_all(&bytes).and_then(|_| stdout.flush()) {
                        result = Err(error.to_string());
                        break 'attach;
                    }
                }
                Response::Exited { code, .. } => {
                    result = Ok(code.unwrap_or(0));
                    if hold_exited_view {
                        exited = true;
                        let _ = writeln!(stdout, "\r\n[Validation exited: {}. Close this tab or press Ctrl+C to leave the read-only output.]\r", code.unwrap_or(0));
                        let _ = stdout.flush();
                    } else {
                        break 'attach;
                    }
                }
                Response::Error(error) => {
                    result = Err(error);
                    break 'attach;
                }
                _ => {}
            }
        }
        if !exited && rx.is_empty() && output_thread.is_finished() {
            result = Err("Session host disconnected; the provider may still be running".into());
            break;
        }
        let new_size = terminal_size();
        if !exited && new_size != size {
            size = new_size;
            if let Err(error) = write_frame(
                &mut stream,
                &Request::Resize {
                    client: client.clone(),
                    epoch: epoch.clone(),
                    cols: size.0,
                    rows: size.1,
                },
            ) {
                result = Err(error);
                break;
            }
        }
        let mut poll = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 15) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            result = Err(std::io::Error::last_os_error().to_string());
            break;
        }
        if poll.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            break;
        }
        if poll.revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(0, input.as_mut_ptr() as *mut libc::c_void, input.len()) };
            if n <= 0 {
                break;
            }
            // Ctrl+] is an explicit detach and never reaches provider stdin.
            if input[..n as usize].contains(&0x1d) {
                break;
            }
            if exited {
                if input[..n as usize].iter().any(|byte| matches!(byte, 3 | 4)) {
                    break;
                }
                continue;
            }
            if let Err(error) = write_frame(
                &mut stream,
                &Request::Input {
                    client: client.clone(),
                    epoch: epoch.clone(),
                    bytes: input[..n as usize].to_vec(),
                },
            ) {
                result = Err(error);
                break;
            }
        }
    }
    let _ = write_frame(&mut stream, &Request::Detach { client, epoch });
    let _ = stream.shutdown(std::net::Shutdown::Both);
    drop(rx);
    let _ = output_thread.join();
    let _=stdout.write_all(b"\x1b[?2026l\x1b[?1049l\x1b[?47l\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1l\x1b>\x1b[<255u\x1b[?25h\x1b[0m\r\n");
    let _ = stdout.flush();
    drop(raw);
    result
}
