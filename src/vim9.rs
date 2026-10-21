//! A native Vim 9 editor session shared by the four terminal workbenches.
//!
//! The frontend owns a real PTY; Vim owns all text, undo, modes and buffers.
//! This module transports a small typed command vocabulary, editor state and
//! opt-in buffer-origin GUI paint frames. It never reconstructs the document,
//! GUI colors or editor commands from terminal output.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[path = "vim9_project.rs"]
mod project;
pub use project::{Vim9ProjectFile, Vim9ProjectIndex, Vim9ProjectSnapshot};

#[path = "vim9_render.rs"]
mod render;
pub use render::{
    Vim9RenderCursor, Vim9RenderFrame, Vim9RenderRect, Vim9RenderRun, Vim9RenderStyle,
    Vim9RenderWindow,
};

#[path = "vim9_gui.rs"]
mod gui;
pub use gui::{
    Vim9BufferPosition, Vim9PixelRect, Vim9RenderAnchor, Vim9RenderMetrics, Vim9UiConfig,
};

#[path = "vim9_entry.rs"]
mod entry;
pub use entry::{explicit_ide_target, Vim9EntryRequest, Vim9EntryTrigger};

const PROTOCOL: u64 = 1;
const MAX_FRAME: usize = 1024 * 1024;
const MAX_COMMAND: usize = 64 * 1024;
const MAX_PENDING: usize = 64;
const MAX_ITEMS: usize = 256;
const TICK: Duration = Duration::from_millis(20);
const AUTH_TIMEOUT: Duration = Duration::from_secs(3);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

const RUNTIME: crate::vendored_script::VendoredScript = crate::vendored_script::VendoredScript {
    name: "vim9-ide-bridge.vim",
    source: include_str!("../runtime/vim9/bridge.vim"),
};

/// Arguments and environment are separate, with no shell evaluation.
#[derive(Clone)]
pub struct Vim9Launch {
    pub program: String,
    pub args: Vec<String>,
    /// Child-local `KEY=value` entries, suitable for VTE's spawn environment.
    pub env: Vec<String>,
    pub cwd: PathBuf,
}

impl std::fmt::Debug for Vim9Launch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vim9Launch")
            .field("program", &self.program)
            .field("argument_count", &self.args.len())
            .field("cwd", &self.cwd)
            .field("env", &"[child-local bridge credentials]")
            .finish()
    }
}

impl Vim9Launch {
    /// For PTY adapters with an argv API but no per-child environment API.
    /// `env` receives each value as one argument, so shell metacharacters in a
    /// project path or Vim executable never become executable shell syntax.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = vec!["/usr/bin/env".into()];
        argv.extend(self.env.iter().cloned());
        argv.push(self.program.clone());
        argv.extend(self.args.iter().cloned());
        argv
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9Buffer {
    pub id: i64,
    pub name: String,
    pub modified: bool,
    pub current: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9Diagnostic {
    pub filename: String,
    pub line: u64,
    pub column: u64,
    pub text: String,
    pub kind: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9Build {
    pub running: bool,
    pub command: String,
    pub exit_code: Option<i32>,
    /// A bounded tail for the native build-output surface.
    pub output: Vec<String>,
    pub cancelled: bool,
    pub truncated: usize,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9Search {
    pub running: bool,
    pub query: String,
    pub count: usize,
    pub truncated: usize,
    /// Ripgrep uses 0 for matches, 1 for no matches, and >1 for errors.
    /// A job that could not be started publishes -1.
    pub exit_code: i32,
    pub errors: Vec<String>,
    pub options: Vim9SearchOptions,
    /// Cancellation retains the previous quickfix list, not partial matches.
    pub cancelled: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Vim9SearchCase {
    /// Ignore case unless the query includes an uppercase character.
    #[default]
    Smart,
    Sensitive,
    Ignore,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9SearchOptions {
    pub case: Vim9SearchCase,
    pub regex: bool,
    pub whole_word: bool,
    /// One optional ripgrep glob, passed as a single literal argv value.
    pub glob: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9Navigation {
    pub can_go_back: bool,
    pub can_go_forward: bool,
}

/// Identity of Vim's current quickfix list. The combined diagnostics may also
/// contain entries from a window location list or language server.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9Quickfix {
    pub source: String,
    pub title: String,
    /// One-based position in the actual quickfix list, zero when it is empty.
    pub index: usize,
    pub count: usize,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9Snapshot {
    pub connected: bool,
    pub mode: String,
    pub file: String,
    pub root: String,
    pub cwd: String,
    pub filetype: String,
    pub encoding: String,
    pub line: u64,
    pub column: u64,
    pub line_count: u64,
    pub window_count: usize,
    /// A save awaits Vim's SafeState event after queued or partially decoded input.
    pub waiting_for_input: bool,
    pub modified: bool,
    pub buffers: Vec<Vim9Buffer>,
    pub diagnostics: Vec<Vim9Diagnostic>,
    pub build: Vim9Build,
    pub search: Vim9Search,
    pub navigation: Vim9Navigation,
    pub quickfix: Vim9Quickfix,
    /// Available optional semantic commands; for example `definition`.
    pub capabilities: Vec<String>,
    pub error: Option<String>,
    pub pending: usize,
    pub buffers_truncated: bool,
    pub diagnostics_truncated: bool,
    /// Independently published native buffer paint data. Cloning a metadata
    /// snapshot does not copy the visible text. Only authenticated render
    /// events may populate this field, never a metadata JSON property.
    #[serde(skip)]
    pub render: Option<Arc<Vim9RenderFrame>>,
}

/// Only these operations can cross the host/editor boundary. In particular
/// there is no `eval`, `ex` or `normal` catch-all. Build is an explicit user
/// action whose shell semantics belong to Vim's configured build workflow.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Vim9Command {
    Open {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        line: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        column: Option<u64>,
    },
    Save,
    SaveAll,
    Buffer {
        buffer: i64,
    },
    Goto {
        line: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        column: Option<u64>,
    },
    CloseBuffer {
        buffer: i64,
    },
    /// Resolve the current buffer in Vim after preceding queued navigation.
    CloseCurrentBuffer,
    NextBuffer,
    PreviousBuffer,
    /// Traverse the current Vim window's native jumplist without mappings.
    NavigateBack,
    NavigateForward,
    Split {
        vertical: bool,
    },
    Build {
        command: String,
    },
    /// Save all modified buffers, then build only if every write succeeds.
    SaveAndBuild {
        command: String,
    },
    CancelBuild,
    Search {
        query: String,
    },
    SearchWithOptions {
        query: String,
        options: Vim9SearchOptions,
    },
    CancelSearch,
    Definition,
    Hover,
    References,
    Rename {
        name: String,
    },
    Format,
    NextDiagnostic,
    PreviousDiagnostic,
    /// Subscribe to buffer-origin GUI paint frames, independently of PTY
    /// output. Existing hosts remain unchanged until they explicitly opt in.
    SetRender {
        enabled: bool,
    },
    Snapshot,
    Quit,
}

impl Vim9Command {
    fn frame(&self, id: u64) -> io::Result<Vec<u8>> {
        match self {
            Self::Open { path, line, column } => {
                if path.is_empty() || path.contains('\0') || *line == Some(0) || *column == Some(0)
                {
                    return Err(invalid(
                        "Open needs a filename and positive line/column numbers",
                    ));
                }
            }
            Self::Buffer { buffer } | Self::CloseBuffer { buffer } if *buffer <= 0 => {
                return Err(invalid("Invalid Vim buffer id"))
            }
            Self::Goto { line, column }
                if *line == 0
                    || *line > i32::MAX as u64
                    || column.is_some_and(|value| value == 0 || value > i32::MAX as u64) =>
            {
                return Err(invalid("Enter a positive line and optional byte column"));
            }
            Self::Search { query } | Self::SearchWithOptions { query, .. }
                if query.is_empty() || query.len() > 4096 || query.contains(['\n', '\r', '\0']) =>
            {
                return Err(invalid(
                    "Enter a single-line search query of at most 4096 bytes",
                ));
            }
            Self::SearchWithOptions { options, .. }
                if options.glob.len() > 4096 || options.glob.contains(['\n', '\r', '\0']) =>
            {
                return Err(invalid(
                    "Enter a single-line file glob of at most 4096 bytes",
                ));
            }
            Self::Rename { name } if name.is_empty() || name.contains(['\n', '\r', '\0']) => {
                return Err(invalid("Enter a single-line symbol name"));
            }
            _ => {}
        }
        let mut value = serde_json::to_value(self).map_err(io::Error::other)?;
        value["id"] = id.into();
        let mut frame = serde_json::to_vec(&value).map_err(io::Error::other)?;
        if frame.len() > MAX_COMMAND {
            return Err(invalid("Vim9 command exceeds the 64 KiB limit"));
        }
        frame.push(b'\n');
        Ok(frame)
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

struct Request {
    id: u64,
    bytes: Vec<u8>,
}

struct Shared {
    entry_id: Option<uuid::Uuid>,
    snapshot: Vim9Snapshot,
    pending: HashSet<u64>,
    // Kept independently of the UI snapshot: metadata may temporarily hide a
    // frame on a mode change without invalidating the ordered wire baseline.
    render_baseline: Option<Arc<Vim9RenderFrame>>,
    render_revision: u64,
    render_resync: bool,
}

struct SessionInner {
    entry_id: uuid::Uuid,
    launch: Vim9Launch,
    requests: Sender<Request>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    next_id: AtomicU64,
}

impl Drop for SessionInner {
    fn drop(&mut self) {
        // Never join a socket worker on a UI thread. Its poll and socket
        // timeouts are bounded, and it does not hold an owning session Arc.
        self.stop.store(true, Ordering::Release);
        set_disconnected(&self.shared, None);
    }
}

/// A cheap cloneable handle. Dropping the last handle disconnects the bridge;
/// the frontend remains responsible for the PTY and graceful close handling.
#[derive(Clone)]
pub struct Vim9Session(Arc<SessionInner>);

impl std::fmt::Debug for Vim9Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Do not include connection credentials in debug dumps.
        f.debug_struct("Vim9Session")
            .field("root", &self.root())
            .field("connected", &self.snapshot().connected)
            .finish_non_exhaustive()
    }
}

impl Vim9Session {
    /// Prepare a Vim9 workbench for a directory or file, including a new file
    /// in an existing directory. The caller must launch the returned command
    /// in a dedicated PTY, never inject it into an existing shell's input.
    pub fn start(request: Vim9EntryRequest) -> io::Result<Self> {
        let (entry_id, target) = request.into_parts();
        let program = find_vim()?;
        let (root, file) = project_target(&target)?;
        let runtime = RUNTIME.path()?;
        Self::prepare(root, file, program, runtime, entry_id)
    }

    fn prepare(
        root: PathBuf,
        file: Option<PathBuf>,
        program: String,
        runtime: PathBuf,
        entry_id: uuid::Uuid,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let env = vec![
            format!("JTERM_VIM9_ADDR={}", listener.local_addr()?),
            format!("JTERM_VIM9_TOKEN={token}"),
            format!("JTERM_VIM9_ROOT={}", path_text(&root)?),
        ];
        let mut args = vec!["-N".into(), "-S".into(), path_text(&runtime)?, "--".into()];
        if let Some(file) = file {
            args.push(path_text(&file)?);
        }
        let mut launch = Vim9Launch {
            program,
            args,
            env,
            cwd: root.clone(),
        };
        if crate::host::is_flatpak() {
            // The cached runtime lives in the app's host-visible cache. Both
            // the Vim executable and build tools must run in the host namespace,
            // as ordinary terminal sessions already do. `env` forwards only
            // this editor's bootstrap values without global environment writes.
            let argv = crate::host::wrap_argv(&launch.argv(), root.to_str(), &[]);
            launch.program.clone_from(&argv[0]);
            launch.args = argv[1..].to_vec();
            launch.env.clear();
        }
        let shared = Arc::new(Mutex::new(Shared {
            entry_id: Some(entry_id),
            snapshot: Vim9Snapshot {
                root: path_text(&root)?,
                ..Default::default()
            },
            pending: HashSet::new(),
            render_baseline: None,
            render_revision: 0,
            render_resync: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = bounded(MAX_PENDING);
        let worker_shared = shared.clone();
        let worker_stop = stop.clone();
        thread::Builder::new()
            .name("vim9-bridge".into())
            .spawn(move || {
                bridge_worker(listener, token, rx, worker_shared, worker_stop);
            })?;
        Ok(Self(Arc::new(SessionInner {
            entry_id,
            launch,
            requests: tx,
            shared,
            stop,
            next_id: AtomicU64::new(1),
        })))
    }

    /// The non-secret identity of this explicit entry, distinct on re-entry.
    pub fn entry_id(&self) -> uuid::Uuid {
        self.0.entry_id
    }

    pub fn is_active(&self) -> bool {
        self.0.shared.lock().entry_id == Some(self.0.entry_id)
    }

    pub fn accepts_entry(&self, entry_id: uuid::Uuid) -> bool {
        self.0.shared.lock().entry_id == Some(entry_id)
    }

    /// Revoke all clones and already queued frames on actual exit. A refused
    /// modified-buffer quit must not call this method.
    pub fn revoke(&self) {
        self.0.stop.store(true, Ordering::Release);
        set_disconnected(&self.0.shared, None);
    }

    pub fn root(&self) -> &Path {
        &self.0.launch.cwd
    }

    pub fn launch(&self) -> &Vim9Launch {
        &self.0.launch
    }

    pub fn launch_argv(&self) -> Vec<String> {
        self.0.launch.argv()
    }

    /// At most 256 buffers/problems and 1 MiB of state are retained. Network
    /// reads and JSON decoding run on the worker rather than the render path.
    pub fn snapshot(&self) -> Vim9Snapshot {
        self.0.shared.lock().snapshot.clone()
    }

    /// Cheap access for a frame-rate native paint loop; does not clone the
    /// buffer list, diagnostics, search results or build output.
    pub fn render_snapshot(&self) -> Option<Arc<Vim9RenderFrame>> {
        self.0.shared.lock().snapshot.render.clone()
    }

    /// Whether a command accepted by `send()` is queued or awaiting its reply.
    /// This does not clone editor metadata or wait for unrelated requests.
    /// Unknown IDs and disconnected sessions return false; a completed command
    /// may have failed, so this is an ordering check rather than a success flag.
    pub fn command_pending(&self, id: u64) -> bool {
        self.0.shared.lock().pending.contains(&id)
    }

    pub fn clear_error(&self) {
        self.0.shared.lock().snapshot.error = None;
    }

    pub fn send(&self, command: Vim9Command) -> io::Result<u64> {
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        let bytes = command.frame(id)?;
        let mut shared = self.0.shared.lock();
        if shared.entry_id != Some(self.0.entry_id) || !shared.snapshot.connected {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Vim9 bridge is not connected",
            ));
        }
        if shared.pending.len() >= MAX_PENDING {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Vim is busy; wait for pending editor commands",
            ));
        }
        // Register before enqueue so an immediate reply cannot race the id.
        shared.pending.insert(id);
        shared.snapshot.pending = shared.pending.len();
        if let Err(error) = self.0.requests.try_send(Request { id, bytes }) {
            shared.pending.remove(&id);
            shared.snapshot.pending = shared.pending.len();
            let kind = match error {
                TrySendError::Full(_) => io::ErrorKind::WouldBlock,
                TrySendError::Disconnected(_) => io::ErrorKind::NotConnected,
            };
            return Err(io::Error::new(kind, "Vim9 command queue is unavailable"));
        }
        shared.snapshot.error = None;
        Ok(id)
    }
}

fn path_text(path: &Path) -> io::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("Vim9 workbench requires UTF-8 paths"))
}

fn find_vim() -> io::Result<String> {
    let configured = match std::env::var("JTERM_VIM9_BIN") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => {
            if crate::host::is_flatpak() {
                "/usr/bin/vim".into()
            } else {
                "vim".into()
            }
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(invalid("JTERM_VIM9_BIN must be UTF-8"));
        }
    };
    if crate::host::is_flatpak() {
        if !Path::new(&configured).is_absolute() || configured.contains('\0') {
            return Err(invalid(
                "JTERM_VIM9_BIN must be an absolute host Vim path in Flatpak",
            ));
        }
        // A sandbox's /usr is different from the host's /usr. The host PTY
        // launch reports a missing host executable; do not stat the wrong one.
        return Ok(configured);
    }
    if let Some(path) =
        crate::host::resolve_configured_program(&configured, std::env::var_os("PATH").as_deref())
    {
        return path_text(&fs::canonicalize(path)?);
    }
    Err(io::Error::new(io::ErrorKind::NotFound,
        "Vim 9 was not found. Install Vim 9 with +eval, +channel, +job and +timers, or set JTERM_VIM9_BIN to its executable."))
}

fn project_target(target: &Path) -> io::Result<(PathBuf, Option<PathBuf>)> {
    let target = if target.as_os_str().is_empty() {
        Path::new(".")
    } else {
        target
    };
    let absolute = if target.is_absolute() {
        target.to_owned()
    } else {
        std::env::current_dir()?.join(target)
    };
    if absolute.is_dir() {
        return Ok((fs::canonicalize(absolute)?, None));
    }
    let filename = absolute
        .file_name()
        .ok_or_else(|| invalid("Choose a project directory or a file"))?;
    let parent = fs::canonicalize(
        absolute
            .parent()
            .ok_or_else(|| invalid("File has no parent directory"))?,
    )?;
    let file = parent.join(filename);
    if file.exists() && !file.is_file() {
        return Err(invalid("The requested editor target is not a regular file"));
    }
    let root = parent
        .ancestors()
        .find(|dir| {
            [
                ".git",
                "Cargo.toml",
                "package.json",
                "pyproject.toml",
                "CMakeLists.txt",
                "Makefile",
            ]
            .iter()
            .any(|marker| dir.join(marker).exists())
        })
        .unwrap_or(&parent)
        .to_owned();
    Ok((root, Some(file)))
}

/// One level of native project navigation. Frontends should perform filesystem
/// I/O on their worker, and display the cap when a directory has 1024 entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

pub fn list_project_directory(path: &Path) -> io::Result<Vec<ProjectEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)?.take(1024) {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let file_type = entry.file_type()?;
        let is_dir = file_type.is_dir() || (file_type.is_symlink() && entry.path().is_dir());
        if is_dir || file_type.is_file() || file_type.is_symlink() {
            entries.push(ProjectEntry {
                path: entry.path(),
                name,
                is_dir,
            });
        }
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(entries)
}

fn set_disconnected(shared: &Mutex<Shared>, error: Option<String>) {
    let mut shared = shared.lock();
    shared.entry_id = None;
    let root = std::mem::take(&mut shared.snapshot.root);
    let prior_error = shared.snapshot.error.take();
    shared.snapshot = Vim9Snapshot {
        root,
        error: prior_error,
        ..Default::default()
    };
    shared.snapshot.render = None;
    shared.render_baseline = None;
    shared.render_resync = false;
    shared.snapshot.pending = 0;
    shared.pending.clear();
    if let Some(error) = error {
        shared.snapshot.error = Some(error);
    }
}

fn bridge_worker(
    listener: TcpListener,
    token: String,
    requests: Receiver<Request>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
) {
    let started = Instant::now();
    let mut announced_timeout = false;
    let mut connected_once = false;
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, address)) if address.ip().is_loopback() => {
                match connection(stream, &token, &requests, &shared, &stop) {
                    Ok(authenticated) => {
                        if authenticated {
                            connected_once = true;
                            set_disconnected(&shared, None);
                            // A PTY belongs to one editor. Never replay requests
                            // to a second Vim after the original process exits.
                            break;
                        }
                    }
                    Err((authenticated, error)) => {
                        if authenticated {
                            connected_once = true;
                            set_disconnected(
                                &shared,
                                Some(format!("Vim9 bridge disconnected: {error}")),
                            );
                            break;
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => {
                set_disconnected(&shared, Some(format!("Vim9 bridge: {error}")));
                break;
            }
        }
        if !announced_timeout && started.elapsed() >= STARTUP_TIMEOUT {
            shared.lock().snapshot.error = Some("Vim9 has not connected. Check the editor output and Vim's +channel, +job and +timers features.".into());
            announced_timeout = true;
        }
        thread::sleep(TICK);
    }
    if !connected_once {
        set_disconnected(&shared, None);
    }
}

fn connection(
    mut stream: TcpStream,
    token: &str,
    requests: &Receiver<Request>,
    shared: &Mutex<Shared>,
    stop: &AtomicBool,
) -> Result<bool, (bool, io::Error)> {
    let mut authenticated = false;
    let mut run = || -> io::Result<bool> {
        stream.set_read_timeout(Some(TICK))?;
        stream.set_write_timeout(Some(Duration::from_millis(250)))?;
        stream.set_nodelay(true)?;
        let deadline = Instant::now() + AUTH_TIMEOUT;
        let mut pending_bytes = Vec::new();
        let mut chunk = [0_u8; 16 * 1024];
        while !stop.load(Ordering::Acquire) {
            if !authenticated && Instant::now() >= deadline {
                return Ok(false);
            }
            if authenticated {
                // Internal, read-only protocol recovery. ID zero cannot
                // acknowledge a user command and is never written to the PTY.
                let resync = std::mem::take(&mut shared.lock().render_resync);
                if resync {
                    stream.write_all(b"{\"action\":\"render_resync\",\"id\":0}\n")?;
                }
                // A fixed budget prevents a producer from starving state reads.
                for request in requests.try_iter().take(8) {
                    if shared.lock().pending.contains(&request.id) {
                        stream.write_all(&request.bytes)?;
                    }
                }
            }
            match stream.read(&mut chunk) {
                Ok(0) => return Ok(authenticated),
                Ok(size) => pending_bytes.extend_from_slice(&chunk[..size]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue
                }
                Err(error) => return Err(error),
            }
            let mut consumed = 0;
            while let Some(offset) = pending_bytes[consumed..]
                .iter()
                .position(|byte| *byte == b'\n')
            {
                let end = consumed + offset;
                if end - consumed > MAX_FRAME {
                    return Err(invalid("Editor state exceeds the 1 MiB frame limit"));
                }
                let value: Value = serde_json::from_slice(&pending_bytes[consumed..end])
                    .map_err(io::Error::other)?;
                consumed = end + 1;
                if !authenticated {
                    if value.get("event").and_then(Value::as_str) != Some("hello")
                        || value.get("protocol").and_then(Value::as_u64) != Some(PROTOCOL)
                        || value.get("token").and_then(Value::as_str) != Some(token)
                    {
                        return Ok(false);
                    }
                    let mut reply = serde_json::to_vec(
                        &json!({"action":"auth","token":token,"protocol":PROTOCOL,"render_protocol":2}),
                    )
                    .map_err(io::Error::other)?;
                    reply.push(b'\n');
                    stream.write_all(&reply)?;
                    authenticated = true;
                    let mut state = shared.lock();
                    if state.entry_id.is_none() {
                        return Ok(false);
                    }
                    state.snapshot.connected = true;
                    state.snapshot.error = None;
                } else {
                    apply_event(value, shared)?;
                }
            }
            if consumed > 0 {
                pending_bytes.drain(..consumed);
            }
            if pending_bytes.len() > MAX_FRAME {
                return Err(invalid("Unterminated editor frame exceeds the 1 MiB limit"));
            }
        }
        Ok(authenticated)
    };
    run().map_err(|error| (authenticated, error))
}

fn apply_event(value: Value, shared: &Mutex<Shared>) -> io::Result<()> {
    let mut shared = shared.lock();
    if shared.entry_id.is_none() {
        return Ok(());
    }
    match value.get("event").and_then(Value::as_str) {
        Some("state") => {
            let mut snapshot: Vim9Snapshot =
                serde_json::from_value(value).map_err(io::Error::other)?;
            snapshot.buffers_truncated |= snapshot.buffers.len() > MAX_ITEMS;
            snapshot.diagnostics_truncated |= snapshot.diagnostics.len() > MAX_ITEMS;
            snapshot.buffers.truncate(MAX_ITEMS);
            snapshot.diagnostics.truncate(MAX_ITEMS);
            snapshot.search.errors.truncate(8);
            if snapshot.build.output.len() > 80 {
                snapshot.build.truncated = snapshot
                    .build
                    .truncated
                    .saturating_add(snapshot.build.output.len() - 80);
                snapshot
                    .build
                    .output
                    .drain(..snapshot.build.output.len() - 80);
            }
            snapshot.capabilities.retain(|capability| {
                matches!(
                    capability.as_str(),
                    "definition" | "hover" | "references" | "rename" | "format"
                )
            });
            snapshot.capabilities.sort();
            snapshot.capabilities.dedup();
            snapshot.connected = true;
            snapshot.pending = shared.pending.len();
            snapshot.error = shared.snapshot.error.take();
            snapshot.render = shared.snapshot.render.take();
            if snapshot.render.as_ref().is_some_and(|frame| {
                frame.mode != snapshot.mode
                    || snapshot
                        .buffers
                        .iter()
                        .find(|buffer| buffer.current)
                        .is_some_and(|buffer| buffer.id != frame.buffer)
            }) {
                snapshot.render = None;
            }
            if snapshot.root.is_empty() {
                snapshot.root.clone_from(&shared.snapshot.root);
            }
            shared.snapshot = snapshot;
        }
        Some("render") => {
            let frame: Vim9RenderFrame = serde_json::from_value(value).map_err(io::Error::other)?;
            if !frame.valid() {
                return Err(invalid("Invalid native Vim buffer paint frame"));
            }
            if frame.revision > shared.render_revision {
                shared.render_revision = frame.revision;
                let frame = Arc::new(frame);
                shared.render_baseline = Some(frame.clone());
                shared.snapshot.render = Some(frame);
                shared.render_resync = false;
            }
        }
        Some("render_delta") => {
            let observed_revision = value.get("revision").and_then(Value::as_u64);
            // Duplicates and delayed old data cannot resurrect an old buffer,
            // even after metadata or compatibility mode hid its native paint.
            if observed_revision.is_some_and(|revision| revision <= shared.render_revision) {
                return Ok(());
            }
            let frame = serde_json::from_value::<render::RenderDelta>(value)
                .ok()
                .and_then(|delta| {
                    shared
                        .render_baseline
                        .as_ref()
                        .and_then(|previous| delta.apply(previous))
                });
            if let Some(frame) = frame {
                shared.render_revision = frame.revision;
                let frame = Arc::new(frame);
                shared.render_baseline = Some(frame.clone());
                shared.snapshot.render = Some(frame);
            } else {
                // Reject atomically; continue ordinary Vim input, metadata and
                // saving while a complete native paint frame is requested.
                // Remember the observed revision even without its baseline so
                // a delayed older keyframe cannot restore stale paint first.
                if let Some(revision) = observed_revision {
                    shared.render_revision = shared.render_revision.max(revision);
                }
                shared.render_baseline = None;
                shared.snapshot.render = None;
                shared.render_resync = true;
            }
        }
        Some("render_disabled") => {
            shared.snapshot.render = None;
            shared.render_baseline = None;
            shared.render_resync = false;
        }
        Some("response") => {
            let id = value
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("Editor response has no request id"))?;
            if shared.pending.remove(&id) {
                shared.snapshot.pending = shared.pending.len();
                if value.get("ok").and_then(Value::as_bool) == Some(false) {
                    shared.snapshot.error = Some(
                        value
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("Vim rejected the command")
                            .to_owned(),
                    );
                }
            }
        }
        Some("error") => {
            shared.snapshot.error = Some(
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Vim9 runtime error")
                    .to_owned(),
            );
        }
        // New informational events are forward-compatible; no data from an
        // unknown event is interpreted as a command or written to the PTY.
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("jterm-vim9-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn session(scratch: &Scratch) -> Vim9Session {
        Vim9Session::prepare(
            scratch.0.clone(),
            None,
            "/usr/bin/vim".into(),
            scratch.0.join("bridge.vim"),
            uuid::Uuid::new_v4(),
        )
        .unwrap()
    }

    #[test]
    fn explicit_exit_rejects_late_events_and_reentry_gets_fresh_identity() {
        let scratch = Scratch::new();
        let first = session(&scratch);
        let first_id = first.entry_id();
        let alias = first.clone();
        apply_event(
            json!({"event":"state","file":"explicit.rs","mode":"n"}),
            &first.0.shared,
        )
        .unwrap();
        assert!(first.is_active());
        assert_eq!(first.snapshot().file, "explicit.rs");
        first.revoke();
        assert!(!alias.is_active());
        assert!(!first.accepts_entry(first_id));
        assert!(!first.snapshot().connected);
        assert!(first.snapshot().file.is_empty());
        for event in [
            json!({"event":"state","file":"late.rs","connected":true,"entry_id":first_id}),
            json!({"event":"render","revision":999}),
            json!({"event":"render_delta","revision":1000}),
            json!({"event":"error","error":"late"}),
        ] {
            apply_event(event, &first.0.shared).unwrap();
        }
        assert_eq!(first.snapshot().file, "");
        assert!(first.render_snapshot().is_none());
        assert!(first.send(Vim9Command::Save).is_err());
        let next = session(&scratch);
        assert_ne!(next.entry_id(), first_id);
        assert!(!next.accepts_entry(first_id));
        apply_event(json!({"event":"state","file":"new.rs"}), &next.0.shared).unwrap();
        assert_eq!(next.snapshot().file, "new.rs");
        assert!(first.snapshot().file.is_empty());
    }

    #[test]
    fn metadata_json_cannot_grant_an_inactive_session() {
        let scratch = Scratch::new();
        let session = session(&scratch);
        let id = session.entry_id();
        session.revoke();
        for event in ["cwd", "title", "hello", "foreground", "restore", "state"] {
            apply_event(json!({"event":event,"cwd":"/tmp/vim","title":"Vim9", "file":"forged", "connected":true,"entry_id":id}), &session.0.shared).unwrap();
            assert!(!session.is_active());
            assert!(!session.snapshot().connected);
            assert!(session.snapshot().file.is_empty());
        }
    }

    fn attach(
        session: &Vim9Session,
        token: Option<&str>,
    ) -> (TcpStream, std::io::BufReader<TcpStream>) {
        let env = &session.launch().env;
        let address = env[0].strip_prefix("JTERM_VIM9_ADDR=").unwrap();
        let own_token = env[1].strip_prefix("JTERM_VIM9_TOKEN=").unwrap();
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let hello = json!({"event":"hello","protocol":1,"token":token.unwrap_or(own_token)});
        writeln!(stream, "{hello}").unwrap();
        let reader = std::io::BufReader::new(stream.try_clone().unwrap());
        (stream, reader)
    }

    #[track_caller]
    fn wait_for(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() {
            assert!(Instant::now() < deadline, "condition timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn file_targets_discover_project_without_losing_new_filename() {
        let scratch = Scratch::new();
        fs::write(scratch.0.join("Cargo.toml"), "").unwrap();
        fs::create_dir(scratch.0.join("src")).unwrap();
        let target = scratch.0.join("src/new file|name.rs");
        let (root, file) = project_target(&target).unwrap();
        assert_eq!(root, scratch.0);
        assert_eq!(file, Some(target));
        assert_eq!(
            project_target(&scratch.0.join("src")).unwrap().0,
            scratch.0.join("src")
        );
    }

    #[test]
    fn argv_keeps_paths_and_environment_as_literal_arguments() {
        let launch = Vim9Launch {
            program: "/tmp/vim binary".into(),
            args: vec!["--".into(), "a;$(touch NO)|b".into()],
            env: vec!["JTERM_VIM9_ROOT=/tmp/project $x".into()],
            cwd: PathBuf::from("/tmp/project $x"),
        };
        assert_eq!(
            launch.argv(),
            [
                "/usr/bin/env",
                "JTERM_VIM9_ROOT=/tmp/project $x",
                "/tmp/vim binary",
                "--",
                "a;$(touch NO)|b"
            ]
        );
    }

    #[test]
    fn native_render_frames_keep_rgb_identity_and_arc_ownership() {
        let shared = Mutex::new(Shared {
            entry_id: Some(uuid::Uuid::new_v4()),
            snapshot: Vim9Snapshot {
                connected: true,
                ..Default::default()
            },
            pending: HashSet::new(),
            render_baseline: None,
            render_revision: 0,
            render_resync: false,
        });
        let frame = json!({
            "event":"render", "revision":2, "supported":true, "mode":"n",
            "rows":30, "columns":120, "window":1000, "buffer":1,
            "view":{"row":0,"column":0,"rows":29,"columns":120},
            "default_style":{"foreground":0x12ab34,"background":0x253647},
            "runs":[{"row":0,"column":0,"text":"界","cells":2,"line":1,
                "byte_start":0,"byte_end":3,"style":{"foreground":0x12ab34,"undercurl":true,"special":0xedab19}}]
        });
        apply_event(frame.clone(), &shared).unwrap();
        let native = shared.lock().snapshot.render.clone().unwrap();
        assert_eq!(native.runs[0].style.foreground, Some(0x12ab34));
        assert_eq!(native.runs[0].cells, 2);
        apply_event(
            json!({"event":"state","mode":"n","buffers":[{"id":1,"current":true}]}),
            &shared,
        )
        .unwrap();
        assert!(Arc::ptr_eq(
            &native,
            shared.lock().snapshot.render.as_ref().unwrap()
        ));
        let mut stale = frame.clone();
        stale["revision"] = 1.into();
        stale["runs"][0]["text"] = "old".into();
        apply_event(stale, &shared).unwrap();
        assert_eq!(
            shared.lock().snapshot.render.as_ref().unwrap().runs[0].text,
            "界"
        );
        apply_event(
            json!({"event":"state","mode":"n","buffers":[{"id":2,"current":true}]}),
            &shared,
        )
        .unwrap();
        assert!(
            shared.lock().snapshot.render.is_none(),
            "never paint another buffer's text"
        );
        apply_event(frame, &shared).unwrap();
        set_disconnected(&shared, None);
        assert!(shared.lock().snapshot.render.is_none());
        let bytes = Vim9Command::SetRender { enabled: true }.frame(8).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["action"],
            "set_render"
        );
    }

    #[test]
    fn native_delta_gap_recovers_without_disconnect_or_replaying_old_paint() {
        let shared = Mutex::new(Shared {
            entry_id: Some(uuid::Uuid::new_v4()),
            snapshot: Vim9Snapshot {
                connected: true,
                ..Default::default()
            },
            pending: HashSet::new(),
            render_baseline: None,
            render_revision: 0,
            render_resync: false,
        });
        let full = json!({"event":"render","revision":1,"supported":true,
            "rows":3,"columns":20,"window":100,"buffer":1,"mode":"n",
            "view":{"row":0,"column":0,"rows":2,"columns":20},
            "runs":[{"row":0,"column":0,"text":"old","cells":3}]});
        apply_event(full.clone(), &shared).unwrap();
        let original = shared.lock().snapshot.render.clone().unwrap();
        let mut delta = full.clone();
        delta.as_object_mut().unwrap().remove("runs");
        delta["event"] = json!("render_delta");
        delta["revision"] = json!(2);
        delta["base_revision"] = json!(1);
        delta["row_updates"] = json!([{"row":0,"runs":[]}]);
        // Metadata can hide paint while the reliable transport baseline stays.
        apply_event(json!({"event":"state","mode":"i"}), &shared).unwrap();
        assert!(shared.lock().snapshot.render.is_none());
        delta["mode"] = json!("i");
        apply_event(delta.clone(), &shared).unwrap();
        assert!(shared
            .lock()
            .snapshot
            .render
            .as_ref()
            .unwrap()
            .runs
            .is_empty());
        assert_eq!(original.runs[0].text, "old");
        delta["revision"] = json!(4);
        delta["base_revision"] = json!(3);
        apply_event(delta.clone(), &shared).unwrap();
        {
            let state = shared.lock();
            assert!(state.snapshot.render.is_none());
            assert!(state.render_resync);
            assert!(state.snapshot.connected);
            assert!(state.snapshot.error.is_none());
        }
        let mut delayed = full.clone();
        delayed["revision"] = json!(3);
        apply_event(delayed, &shared).unwrap();
        {
            let state = shared.lock();
            assert!(
                state.snapshot.render.is_none(),
                "a keyframe older than the failed delta cannot restore paint"
            );
            assert!(state.render_baseline.is_none());
            assert_eq!(state.render_revision, 4);
            assert!(state.render_resync);
        }
        let mut recovered = full.clone();
        recovered["revision"] = json!(5);
        recovered["mode"] = json!("i");
        recovered["runs"][0]["text"] = json!("new");
        apply_event(recovered.clone(), &shared).unwrap();
        let restored = shared.lock().snapshot.render.clone().unwrap();
        assert!(!shared.lock().render_resync);
        apply_event(delta, &shared).unwrap();
        apply_event(full, &shared).unwrap();
        assert!(Arc::ptr_eq(
            &restored,
            shared.lock().snapshot.render.as_ref().unwrap()
        ));
        apply_event(json!({"event":"render_disabled"}), &shared).unwrap();
        apply_event(recovered, &shared).unwrap();
        assert!(
            shared.lock().snapshot.render.is_none(),
            "old keyframe cannot undo disable"
        );
    }

    #[test]
    fn fragmented_delta_gap_requests_keyframe_and_preserves_command_channel() {
        let scratch = Scratch::new();
        let editor = session(&scratch);
        let (mut writer, mut reader) = attach(&editor, None);
        let mut wire = String::new();
        reader.read_line(&mut wire).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&wire).unwrap()["render_protocol"],
            2
        );
        let full = json!({"event":"render","revision":1,"supported":true,
            "rows":3,"columns":20,"window":100,"buffer":1,"mode":"n",
            "view":{"row":0,"column":0,"rows":2,"columns":20},
            "runs":[{"row":0,"column":0,"text":"界","cells":2}]});
        writeln!(writer, "{full}").unwrap();
        wait_for(|| editor.render_snapshot().is_some());
        let mut delta = full.clone();
        delta.as_object_mut().unwrap().remove("runs");
        delta["event"] = json!("render_delta");
        delta["base_revision"] = json!(2);
        delta["revision"] = json!(3);
        delta["row_updates"] = json!([]);
        for bytes in format!("{delta}\n").as_bytes().chunks(7) {
            writer.write_all(bytes).unwrap();
        }
        wire.clear();
        reader.read_line(&mut wire).unwrap();
        let recovery: Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(recovery["action"], "render_resync");
        assert_eq!(recovery["id"], 0);
        assert!(editor.snapshot().connected);
        assert!(editor.render_snapshot().is_none());
        let mut recovered = full;
        recovered["revision"] = json!(4);
        writeln!(writer, "{recovered}").unwrap();
        wait_for(|| {
            editor
                .render_snapshot()
                .is_some_and(|frame| frame.revision == 4)
        });
        editor.send(Vim9Command::Save).unwrap();
        wire.clear();
        reader.read_line(&mut wire).unwrap();
        let save: Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(save["action"], "save");
        assert!(save["id"].as_u64().unwrap() > 0);
        writeln!(
            writer,
            "{}",
            json!({"event":"response","id":save["id"],"ok":true})
        )
        .unwrap();
        wait_for(|| editor.snapshot().pending == 0);
    }

    #[test]
    fn native_render_rejects_invalid_geometry_colors_and_embedded_control_text() {
        let shared = Mutex::new(Shared {
            entry_id: Some(uuid::Uuid::new_v4()),
            snapshot: Vim9Snapshot::default(),
            pending: HashSet::new(),
            render_baseline: None,
            render_revision: 0,
            render_resync: false,
        });
        let frame = json!({"event":"render","revision":1,"supported":true,
            "rows":30,"columns":120,"window":1000,"buffer":1,
            "view":{"row":0,"column":0,"rows":29,"columns":120},
            "runs":[{"row":0,"column":119,"text":"x","cells":1,"line":1,
                "byte_start":0,"byte_end":1,"style":{"foreground":0x123456}}]});
        for (name, value) in [
            ("column", json!(120)),
            ("cells", json!(usize::MAX)),
            ("text", json!("\u{1b}[31m")),
        ] {
            let mut malformed = frame.clone();
            malformed["runs"][0][name] = value;
            assert!(apply_event(malformed, &shared).is_err(), "{name}");
        }
        let mut invalid_color = frame.clone();
        invalid_color["runs"][0]["style"]["foreground"] = json!(0x1000000);
        assert!(apply_event(invalid_color, &shared).is_err());
        assert!(shared.lock().snapshot.render.is_none());
        apply_event(frame, &shared).unwrap();
        apply_event(json!({"event":"render_disabled"}), &shared).unwrap();
        assert!(shared.lock().snapshot.render.is_none());
    }

    #[test]
    fn command_serialization_cannot_turn_filename_into_an_action() {
        let filename = "a\"\n{\"action\":\"build\"}|%#.rs";
        let bytes = Vim9Command::Open {
            path: filename.into(),
            line: Some(12),
            column: None,
        }
        .frame(7)
        .unwrap();
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["path"], filename);
        assert_eq!(value["action"], "open");
        assert_eq!(value["id"], 7);
        assert!(Vim9Command::Buffer { buffer: 0 }.frame(1).is_err());
        assert!(Vim9Command::Search {
            query: String::new()
        }
        .frame(1)
        .is_err());
        assert!(Vim9Command::Build {
            command: "x".repeat(MAX_COMMAND)
        }
        .frame(1)
        .is_err());
    }

    #[test]
    fn navigation_actions_validate_positions_and_buffer_identity() {
        for command in [
            Vim9Command::Goto {
                line: 0,
                column: None,
            },
            Vim9Command::Goto {
                line: 1,
                column: Some(0),
            },
            Vim9Command::Goto {
                line: u64::MAX,
                column: None,
            },
            Vim9Command::CloseBuffer { buffer: -1 },
        ] {
            assert!(command.frame(1).is_err());
        }
        for (command, action) in [
            (
                Vim9Command::Goto {
                    line: 24,
                    column: Some(3),
                },
                "goto",
            ),
            (Vim9Command::CloseBuffer { buffer: 3 }, "close_buffer"),
            (Vim9Command::CloseCurrentBuffer, "close_current_buffer"),
            (Vim9Command::NextBuffer, "next_buffer"),
            (Vim9Command::PreviousBuffer, "previous_buffer"),
            (Vim9Command::NavigateBack, "navigate_back"),
            (Vim9Command::NavigateForward, "navigate_forward"),
            (Vim9Command::CancelSearch, "cancel_search"),
            (Vim9Command::Split { vertical: true }, "split"),
            (
                Vim9Command::SaveAndBuild {
                    command: "cargo check".into(),
                },
                "save_and_build",
            ),
        ] {
            let value: Value = serde_json::from_slice(&command.frame(42).unwrap()).unwrap();
            assert_eq!(value["action"], action);
            assert_eq!(value["id"], 42);
        }
    }

    #[test]
    fn search_options_preserve_literals_and_validate_before_transport() {
        let options = Vim9SearchOptions {
            case: Vim9SearchCase::Ignore,
            regex: true,
            whole_word: true,
            glob: "a file*.rs;$(touch NEVER)".into(),
        };
        let query = "--Needle|内容";
        let value: Value = serde_json::from_slice(
            &Vim9Command::SearchWithOptions {
                query: query.into(),
                options: options.clone(),
            }
            .frame(8)
            .unwrap(),
        )
        .unwrap();
        assert_eq!(value["action"], "search_with_options");
        assert_eq!(value["query"], query);
        assert_eq!(value["options"]["glob"], options.glob);
        assert_eq!(value["options"]["case"], "ignore");
        assert_eq!(value["options"]["regex"], true);
        assert_eq!(value["options"]["whole_word"], true);
        for query in [
            "".into(),
            "two\nlines".into(),
            "zero\0byte".into(),
            "x".repeat(4097),
        ] {
            assert!(Vim9Command::Search {
                query: query.clone()
            }
            .frame(1)
            .is_err());
            assert!(Vim9Command::SearchWithOptions {
                query,
                options: options.clone()
            }
            .frame(1)
            .is_err());
        }
        for glob in ["two\rrows".into(), "zero\0byte".into(), "x".repeat(4097)] {
            assert!(Vim9Command::SearchWithOptions {
                query: "valid".into(),
                options: Vim9SearchOptions {
                    glob,
                    ..Vim9SearchOptions::default()
                },
            }
            .frame(1)
            .is_err());
        }
        let defaults: Vim9SearchOptions = serde_json::from_value(json!({})).unwrap();
        assert_eq!(defaults.case, Vim9SearchCase::Smart);
        assert!(!defaults.regex && !defaults.whole_word && defaults.glob.is_empty());
    }

    #[test]
    fn navigation_and_result_metadata_decode_with_old_snapshot_defaults() {
        let legacy: Vim9Snapshot = serde_json::from_value(json!({"file":"legacy.txt"})).unwrap();
        assert!(!legacy.navigation.can_go_back);
        assert!(!legacy.navigation.can_go_forward);
        assert_eq!(legacy.search.options.case, Vim9SearchCase::Smart);
        assert!(!legacy.search.cancelled);
        let snapshot: Vim9Snapshot = serde_json::from_value(json!({
            "navigation":{"can_go_back":true,"can_go_forward":false},
            "quickfix":{"source":"build","title":"Build: cargo check","index":2,"count":4},
            "search":{"cancelled":true,"options":{"case":"sensitive","regex":true,"whole_word":true,"glob":"*.rs"}}
        })).unwrap();
        assert!(snapshot.navigation.can_go_back);
        assert!(!snapshot.navigation.can_go_forward);
        assert_eq!(snapshot.quickfix.source, "build");
        assert_eq!((snapshot.quickfix.index, snapshot.quickfix.count), (2, 4));
        assert!(snapshot.search.cancelled);
        assert_eq!(snapshot.search.options.case, Vim9SearchCase::Sensitive);
        assert!(snapshot.search.options.regex && snapshot.search.options.whole_word);
    }

    #[test]
    fn editor_metadata_and_build_outcome_survive_the_protocol_boundary() {
        let shared = Mutex::new(Shared {
            entry_id: Some(uuid::Uuid::new_v4()),
            snapshot: Vim9Snapshot::default(),
            pending: HashSet::new(),
            render_baseline: None,
            render_revision: 0,
            render_resync: false,
        });
        apply_event(json!({"event":"state", "cwd":"/project/src", "filetype":"rust", "encoding":"utf-8", "line_count":500, "window_count":2, "build":{"running":false, "cancelled":true, "truncated":23}}), &shared).unwrap();
        let snapshot = &shared.lock().snapshot;
        assert_eq!(snapshot.cwd, "/project/src");
        assert_eq!(snapshot.filetype, "rust");
        assert_eq!(snapshot.encoding, "utf-8");
        assert_eq!(snapshot.line_count, 500);
        assert_eq!(snapshot.window_count, 2);
        assert!(snapshot.build.cancelled);
        assert_eq!(snapshot.build.truncated, 23);
    }

    #[test]
    fn wrong_client_cannot_publish_state_and_valid_client_can_still_connect() {
        let scratch = Scratch::new();
        let session = session(&scratch);
        let (mut wrong, mut wrong_reader) = attach(&session, Some("wrong"));
        let _ = writeln!(wrong, "{}", json!({"event":"state","file":"spoofed"}));
        let mut reply = String::new();
        let _ = wrong_reader.read_line(&mut reply);
        assert!(reply.is_empty());
        assert!(!session.snapshot().connected);
        assert!(session.snapshot().file.is_empty());
        let (_stream, mut reader) = attach(&session, None);
        reader.read_line(&mut reply).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap()["action"],
            "auth"
        );
        wait_for(|| session.snapshot().connected);
    }

    #[test]
    fn fragmented_frames_and_responses_update_the_actual_session() {
        let scratch = Scratch::new();
        let session = session(&scratch);
        let (mut stream, mut reader) = attach(&session, None);
        let mut auth = String::new();
        reader.read_line(&mut auth).unwrap();
        wait_for(|| session.snapshot().connected);
        let frame = format!(
            "{}\n",
            json!({"event":"state","mode":"n","file":"a.rs","modified":true,"line":11,"buffers":[{"id":3,"name":"a.rs","current":true,"modified":true}]})
        );
        for chunk in frame.as_bytes().chunks(3) {
            stream.write_all(chunk).unwrap();
        }
        wait_for(|| session.snapshot().file == "a.rs");
        assert!(session.snapshot().modified);
        let id = session.send(Vim9Command::Save).unwrap();
        assert_eq!(session.snapshot().pending, 1);
        assert!(session.command_pending(id));
        assert!(!session.command_pending(id + 1));
        let other_id = session.send(Vim9Command::Snapshot).unwrap();
        assert!(session.command_pending(other_id));
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        let command: Value = serde_json::from_str(&request).unwrap();
        assert_eq!(command["action"], "save");
        assert_eq!(command["id"], id);
        writeln!(
            stream,
            "{}",
            json!({"event":"response","id":id,"ok":false,"error":"E45: readonly"})
        )
        .unwrap();
        wait_for(|| !session.command_pending(id));
        assert!(session.command_pending(other_id));
        assert_eq!(session.snapshot().pending, 1);
        assert_eq!(session.snapshot().error.as_deref(), Some("E45: readonly"));
        drop(reader);
        drop(stream);
        wait_for(|| !session.snapshot().connected);
        assert!(!session.command_pending(other_id));
        assert!(session.send(Vim9Command::Save).is_err());
    }

    #[test]
    fn oversized_unterminated_state_disconnects_without_unbounded_memory() {
        let scratch = Scratch::new();
        let session = session(&scratch);
        let (mut stream, mut reader) = attach(&session, None);
        reader.read_line(&mut String::new()).unwrap();
        wait_for(|| session.snapshot().connected);
        let _ = stream.write_all(&vec![b'x'; MAX_FRAME + 1]);
        wait_for(|| !session.snapshot().connected);
        assert!(session.snapshot().error.unwrap().contains("1 MiB"));
    }

    #[test]
    fn pending_commands_are_bounded_and_not_replayed_after_disconnect() {
        let scratch = Scratch::new();
        let session = session(&scratch);
        let (stream, mut reader) = attach(&session, None);
        reader.read_line(&mut String::new()).unwrap();
        wait_for(|| session.snapshot().connected);
        for _ in 0..MAX_PENDING {
            session.send(Vim9Command::Save).unwrap();
        }
        assert_eq!(
            session.send(Vim9Command::Save).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(reader);
        drop(stream);
        wait_for(|| !session.snapshot().connected);
        assert_eq!(session.snapshot().pending, 0);
    }

    #[test]
    fn snapshots_cap_items_and_only_publish_supported_semantic_actions() {
        let shared = Mutex::new(Shared {
            entry_id: Some(uuid::Uuid::new_v4()),
            snapshot: Vim9Snapshot::default(),
            pending: HashSet::new(),
            render_baseline: None,
            render_revision: 0,
            render_resync: false,
        });
        apply_event(json!({"event":"state","buffers":vec![json!({"id":1});300],"capabilities":["eval","definition","definition","format"],"search":{"query":"needle","count":256,"truncated":3,"exit_code":2,"errors":vec!["search failed";12]}}), &shared).unwrap();
        let snapshot = &shared.lock().snapshot;
        assert_eq!(snapshot.buffers.len(), MAX_ITEMS);
        assert!(snapshot.buffers_truncated);
        assert_eq!(snapshot.capabilities, ["definition", "format"]);
        assert_eq!(snapshot.search.query, "needle");
        assert_eq!(snapshot.search.exit_code, 2);
        assert_eq!(snapshot.search.truncated, 3);
        assert_eq!(snapshot.search.errors.len(), 8);
    }

    #[test]
    fn project_navigation_lists_directories_first_without_recursion() {
        let scratch = Scratch::new();
        fs::write(scratch.0.join("a.rs"), "").unwrap();
        fs::create_dir(scratch.0.join("z-dir")).unwrap();
        fs::write(scratch.0.join("z-dir/hidden.rs"), "").unwrap();
        let entries = list_project_directory(&scratch.0).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "z-dir");
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].name, "a.rs");
    }

    /// The Python runtime suite exercises Vim's editing semantics; this test
    /// additionally sends those real messages through the production Rust
    /// decoder, catching incompatible bool/number/field changes at the seam.
    #[test]
    #[cfg(unix)]
    #[ignore = "requires Vim 9, a PTY and loopback sockets; set VIM9_TEST_VIM"]
    fn real_vim9_round_trip() {
        use std::os::fd::FromRawFd;
        use std::os::unix::process::CommandExt;

        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let scratch = Scratch::new();
        let first = scratch.0.join("native.txt");
        let second = scratch.0.join("space | '%.txt");
        fs::write(&first, "one\ntwo\n").unwrap();
        fs::write(&second, "three\nfour\n").unwrap();
        let vimrc = scratch.0.join("user.vimrc");
        fs::write(
            &vimrc,
            "set nocompatible nomore noswapfile\nset errorformat=%f:%l:%c:%m\nset shortmess+=I\n",
        )
        .unwrap();
        let runtime = RUNTIME.materialize(&scratch.0.join("runtime")).unwrap();
        let program = std::env::var("VIM9_TEST_VIM").unwrap_or_else(|_| find_vim().unwrap());
        let editor = Vim9Session::prepare(
            scratch.0.clone(),
            Some(first.clone()),
            program,
            runtime,
            uuid::Uuid::new_v4(),
        )
        .unwrap();
        let mut master = -1;
        let mut slave = -1;
        let size = libc::winsize {
            ws_row: 30,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: output descriptors and window-size pointers are valid.
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
        // SAFETY: openpty returned two owned descriptors, consumed once here.
        let mut master = unsafe { fs::File::from_raw_fd(master) };
        let slave = unsafe { fs::File::from_raw_fd(slave) };
        let mut command = std::process::Command::new(&editor.launch().program);
        command
            .args(["-u", vimrc.to_str().unwrap(), "-i", "NONE", "-n"])
            .args(&editor.launch().args)
            .current_dir(editor.root())
            .env("TERM", "xterm-256color")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave);
        for entry in &editor.launch().env {
            let (name, value) = entry.split_once('=').unwrap();
            command.env(name, value);
        }
        // SAFETY: only async-signal-safe libc operations run between fork and
        // exec. The child receives a separate controlling terminal/session.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = Child(command.spawn().unwrap());
        let mut drain = master.try_clone().unwrap();
        thread::spawn(move || {
            let mut chunk = [0_u8; 4096];
            while matches!(drain.read(&mut chunk), Ok(1..)) {}
        });
        wait_for(|| editor.snapshot().file == first.to_str().unwrap());
        assert!(editor.snapshot().connected);
        assert!(!editor.snapshot().modified);
        assert!(!editor.snapshot().buffers_truncated);
        editor
            .send(Vim9Command::SetRender { enabled: true })
            .unwrap();
        wait_for(|| editor.render_snapshot().is_some());
        let first_frame = editor.render_snapshot().unwrap();
        assert!(first_frame.supported, "{}", first_frame.fallback_reason);
        assert!(first_frame.runs.iter().any(|run| run.text == "one"));
        let id = editor
            .send(Vim9Command::Open {
                path: second.to_str().unwrap().into(),
                line: Some(2),
                column: Some(2),
            })
            .unwrap();
        wait_for(|| {
            editor.snapshot().file == second.to_str().unwrap() && editor.snapshot().pending == 0
        });
        assert!(id > 0);
        assert_eq!(editor.snapshot().line, 2);
        assert_eq!(editor.snapshot().column, 2);
        wait_for(|| {
            editor
                .render_snapshot()
                .is_some_and(|frame| frame.buffer != first_frame.buffer)
        });
        assert!(editor
            .render_snapshot()
            .unwrap()
            .runs
            .iter()
            .any(|run| run.text == "four"));

        master.write_all(b"iX\x1b").unwrap();
        wait_for(|| {
            let snapshot = editor.snapshot();
            snapshot.modified && snapshot.mode == "n"
        });
        editor.send(Vim9Command::Quit).unwrap();
        wait_for(|| editor.snapshot().error.is_some());
        let quit_error = editor.snapshot().error.unwrap();
        assert!(
            quit_error.contains("E37") || quit_error.contains("E162"),
            "{quit_error}"
        );
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "dirty Vim must remain alive"
        );
        editor.send(Vim9Command::Save).unwrap();
        wait_for(|| editor.snapshot().pending == 0 && !editor.snapshot().modified);
        assert_eq!(fs::read_to_string(&second).unwrap(), "three\nfXour\n");

        editor
            .send(Vim9Command::Build {
                command: "printf 'native.txt:2:1:typed bridge check\\n'; exit 3".into(),
            })
            .unwrap();
        wait_for(|| {
            editor.snapshot().build.exit_code == Some(3) && !editor.snapshot().build.running
        });
        let snapshot = editor.snapshot();
        assert_eq!(snapshot.build.output, ["native.txt:2:1:typed bridge check"]);
        assert_eq!(snapshot.diagnostics.len(), 1);
        assert_eq!(snapshot.diagnostics[0].line, 2);
        editor
            .send(Vim9Command::Goto {
                line: 1,
                column: Some(3),
            })
            .unwrap();
        wait_for(|| editor.snapshot().pending == 0 && editor.snapshot().line == 1);
        assert_eq!(editor.snapshot().column, 3);
        assert_eq!(editor.snapshot().line_count, 2);
        assert_eq!(editor.snapshot().encoding, "utf-8");
        editor.send(Vim9Command::PreviousBuffer).unwrap();
        wait_for(|| {
            editor.snapshot().pending == 0 && editor.snapshot().file == first.to_str().unwrap()
        });
        editor.send(Vim9Command::NextBuffer).unwrap();
        wait_for(|| {
            editor.snapshot().pending == 0 && editor.snapshot().file == second.to_str().unwrap()
        });
        assert_eq!(editor.snapshot().column, 3);
        editor.send(Vim9Command::Split { vertical: true }).unwrap();
        wait_for(|| editor.snapshot().pending == 0 && editor.snapshot().window_count == 2);
        master.write_all(b"iY\x1b").unwrap();
        wait_for(|| editor.snapshot().modified && editor.snapshot().mode == "n");
        editor
            .send(Vim9Command::SaveAndBuild {
                command: "printf 'saved build\\n'".into(),
            })
            .unwrap();
        wait_for(|| editor.snapshot().pending == 0 && editor.snapshot().build.exit_code == Some(0));
        assert_eq!(fs::read_to_string(&second).unwrap(), "thYree\nfXour\n");
        let current = editor
            .snapshot()
            .buffers
            .into_iter()
            .find(|buffer| buffer.current)
            .unwrap()
            .id;
        editor
            .send(Vim9Command::CloseBuffer { buffer: current })
            .unwrap();
        wait_for(|| {
            editor.snapshot().pending == 0
                && !editor
                    .snapshot()
                    .buffers
                    .iter()
                    .any(|buffer| buffer.id == current)
        });
        editor.send(Vim9Command::Quit).unwrap();
        wait_for(|| child.0.try_wait().unwrap().is_some());
    }
}
